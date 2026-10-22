use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{DefaultBodyLimit, State};
use axum::http::header::{AUTHORIZATION, CONTENT_TYPE};
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use chrono::Utc;
use futures_util::StreamExt;
use serde_json::{Value, json};
use tokio::sync::{mpsc, oneshot};
use tokio_stream::wrappers::ReceiverStream;

use crate::config::{self, Budgets, FailMode, Mode, Resolved};
use crate::ledger::{
    self, DAILY_TTL_SECS, Keys, Ledger, LimitSpec, MONTHLY_TTL_SECS, RUN_TTL_SECS, Reservation,
    Scope,
};
use crate::pricing::Price;
use crate::providers::{self, Api, StreamTracker, Usage};

pub struct AppState {
    /// Current config; replaced when orgs, keys or budgets change from the admin API.
    rt: RwLock<Arc<Resolved>>,
    /// Version of the managed config in Redis that `rt` reflects (0 = not managed yet).
    pub control_version: AtomicU64,
    /// Serialises config edits made through this process.
    pub control_edit: tokio::sync::Mutex<()>,
    pub ledger: Ledger,
    /// Postgres, when configured: config history and the audit log.
    pub db: Option<crate::db::Db>,
    pub http: reqwest::Client,
    /// Request events for the dashboard, written to Redis by a background task.
    pub activity: mpsc::Sender<Value>,
}

impl AppState {
    pub fn new(
        rt: Resolved,
        ledger: Ledger,
        db: Option<crate::db::Db>,
        http: reqwest::Client,
        activity: mpsc::Sender<Value>,
    ) -> Self {
        Self {
            rt: RwLock::new(Arc::new(rt)),
            control_version: AtomicU64::new(0),
            control_edit: tokio::sync::Mutex::new(()),
            ledger,
            db,
            http,
            activity,
        }
    }

    pub fn rt(&self) -> Arc<Resolved> {
        self.rt.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub fn replace_rt(&self, rt: Resolved, version: u64) {
        *self.rt.write().unwrap_or_else(|e| e.into_inner()) = Arc::new(rt);
        self.control_version.store(version, Ordering::SeqCst);
    }
}

/// Input tokens assumed for a Responses API turn whose stored context we have never seen.
const UNKNOWN_CONTEXT_TOKENS: u64 = 32_000;
/// Reservations outlive the longest possible upstream request by this much, so a live request is never swept.
const RESERVATION_GRACE_SECS: u64 = 120;
const RESERVATION_RECORD_EXTRA_SECS: u64 = 24 * 3600;

pub fn router(state: Arc<AppState>) -> Router {
    let limit = state.rt().cfg.server.max_body_bytes;
    Router::new()
        .route("/v1/chat/completions", post(openai_chat))
        .route("/v1/responses", post(openai_responses))
        .route("/v1/messages", post(anthropic_messages))
        // TokenLens out of the request path: reported usage, budget checks, agent telemetry.
        .route("/v1/usage", post(crate::ingest::usage))
        .route("/v1/check", post(crate::ingest::check))
        .route("/v1/otel/v1/logs", post(crate::ingest::otel_logs))
        .route("/v1/otel/v1/traces", post(crate::ingest::otel_discard))
        .route("/v1/otel/v1/metrics", post(crate::ingest::otel_discard))
        .route(
            "/v1/hooks/claude-code",
            post(crate::ingest::claude_code_hook),
        )
        .route("/v1/hooks/codex", post(crate::ingest::codex_hook))
        .route("/v1/hooks/gemini", post(crate::ingest::gemini_hook))
        .route("/v1/hooks/cursor", post(crate::ingest::cursor_hook))
        .route("/healthz", get(|| async { "ok" }))
        .layer(DefaultBodyLimit::max(limit))
        .with_state(state)
}

async fn openai_chat(State(st): State<Arc<AppState>>, headers: HeaderMap, body: Bytes) -> Response {
    handle(st, Api::OpenAIChat, headers, body).await
}

async fn openai_responses(
    State(st): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    handle(st, Api::OpenAIResponses, headers, body).await
}

async fn anthropic_messages(
    State(st): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    handle(st, Api::AnthropicMessages, headers, body).await
}

#[derive(Clone)]
struct Ctx {
    request_id: String,
    org: String,
    project: String,
    agent: String,
    user: Option<String>,
    run_id: String,
    parent_run_id: Option<String>,
    api: Api,
    model: String,
    stream: bool,
    price: Price,
    price_known: bool,
    input_est: u64,
    max_output: u64,
    estimate: u64,
    scopes: Vec<Scope>,
    check_exceeded: Vec<String>,
    degraded: bool,
    pre_upstream_us: u64,
}

struct Outcome<'a> {
    decision: &'a str,
    blocked_scope: Option<&'a str>,
    usage: Option<Usage>,
    usage_complete: bool,
    actual: u64,
    upstream_status: Option<u16>,
    upstream_ms: u64,
    client_disconnected: bool,
}

impl Outcome<'_> {
    fn blocked<'a>(decision: &'a str, scope: &'a str) -> Outcome<'a> {
        Outcome {
            decision,
            blocked_scope: Some(scope),
            usage: None,
            usage_complete: false,
            actual: 0,
            upstream_status: None,
            upstream_ms: 0,
            client_disconnected: false,
        }
    }
}

async fn handle(st: Arc<AppState>, api: Api, headers: HeaderMap, body: Bytes) -> Response {
    let started = Instant::now();
    let request_id = format!("req_{}", uuid::Uuid::new_v4().simple());
    let reject = |status: StatusCode, kind: &str, message: &str| {
        error_response(api, status, kind, message, &request_id)
    };

    let Some(token) = client_token(&headers) else {
        return reject(
            StatusCode::UNAUTHORIZED,
            "tokenlens_unauthorized",
            "missing TokenLens virtual key (send it as 'Authorization: Bearer' or 'x-api-key')",
        );
    };
    let rt = st.rt();
    let Some(&(oi, pi)) = rt.keys.get(&config::sha256_hex(token)) else {
        return reject(
            StatusCode::UNAUTHORIZED,
            "tokenlens_unauthorized",
            "invalid TokenLens virtual key",
        );
    };
    let org = &rt.cfg.orgs[oi];
    let project = &org.projects[pi];

    let run_id = match header_id(&headers, "x-tl-run-id") {
        Ok(r) => r.unwrap_or_else(|| request_id.clone()),
        Err(m) => return reject(StatusCode::BAD_REQUEST, "tokenlens_invalid_request", &m),
    };
    let parent = match header_id(&headers, "x-tl-parent-run-id") {
        Ok(p) => p,
        Err(m) => return reject(StatusCode::BAD_REQUEST, "tokenlens_invalid_request", &m),
    };
    if parent.as_deref() == Some(run_id.as_str()) {
        return reject(
            StatusCode::BAD_REQUEST,
            "tokenlens_invalid_request",
            "a run cannot be its own parent",
        );
    }
    let agent = match header_id(&headers, "x-tl-agent") {
        Ok(a) => a.unwrap_or_else(|| "default".into()),
        Err(m) => return reject(StatusCode::BAD_REQUEST, "tokenlens_invalid_request", &m),
    };
    // The person or service on whose behalf the agent runs; for spend reports only.
    let user = match headers.get("x-tl-user").map(|v| v.to_str().map(str::trim)) {
        None => None,
        Some(Ok(u)) if config::valid_user(u) => Some(u.to_string()),
        Some(_) => {
            return reject(
                StatusCode::BAD_REQUEST,
                "tokenlens_invalid_request",
                "x-tl-user must be 1-128 letters, digits, '.', '_', '-', '@' or '+'",
            );
        }
    };
    let run_budget_header = match headers.get("x-tl-run-budget-usd") {
        None => None,
        Some(v) => match v
            .to_str()
            .ok()
            .and_then(|s| s.trim().parse::<f64>().ok())
            .filter(|x| x.is_finite() && *x > 0.0)
        {
            Some(x) => Some(x),
            None => {
                return reject(
                    StatusCode::BAD_REQUEST,
                    "tokenlens_invalid_request",
                    "x-tl-run-budget-usd must be a positive number of US dollars",
                );
            }
        },
    };

    let prepared = match providers::prepare(api, &body, rt.cfg.server.default_max_output_tokens) {
        Ok(p) => p,
        Err(m) => return reject(StatusCode::BAD_REQUEST, "tokenlens_invalid_request", &m),
    };
    let provider = api.provider();
    let Some(base) = rt.upstream_base(provider) else {
        return reject(
            StatusCode::BAD_REQUEST,
            "tokenlens_provider_not_configured",
            &format!(
                "provider {} is not configured on this proxy",
                provider.name()
            ),
        );
    };
    let Some(api_key) = rt.provider_key(oi, provider) else {
        return reject(
            StatusCode::BAD_REQUEST,
            "tokenlens_provider_not_configured",
            &format!(
                "no {} API key for org {}: add one in the TokenLens dashboard",
                provider.name(),
                org.id
            ),
        );
    };
    let mut upstream_headers = match providers::upstream_headers(api, &headers, api_key) {
        Ok(h) => h,
        Err(m) => {
            return reject(
                StatusCode::INTERNAL_SERVER_ERROR,
                "tokenlens_internal_error",
                &m,
            );
        }
    };

    if matches!(provider, providers::Provider::Anthropic)
        && let Some(ws) = org.anthropic_workspace_id.as_deref()
        && !upstream_headers.contains_key("anthropic-workspace-id")
        && let Ok(v) = HeaderValue::from_str(ws)
    {
        upstream_headers.insert(HeaderName::from_static("anthropic-workspace-id"), v);
    }

    let closed = org.fail_mode == FailMode::Closed;
    let unavailable = || {
        error_response(
            api,
            StatusCode::SERVICE_UNAVAILABLE,
            "tokenlens_unavailable",
            "budget ledger unavailable and this org fails closed",
            &request_id,
        )
    };
    let mut degraded = false;

    let (price, price_known) = rt.prices.lookup(provider, &prepared.model);
    let mut input_est = prepared.input_tokens_est;
    if let Some(prev) = prepared.previous_response_id.as_deref() {
        let known = if config::valid_id(prev) {
            st.ledger.response_context_tokens(&org.id, prev).await
        } else {
            Ok(None)
        };
        match known {
            Ok(Some(t)) => input_est += t,
            Ok(None) => input_est += UNKNOWN_CONTEXT_TOKENS,
            Err(e) => {
                tracing::error!(request_id, error = %e, "response context lookup failed");
                if closed {
                    return unavailable();
                }
                degraded = true;
                input_est += UNKNOWN_CONTEXT_TOKENS;
            }
        }
    }
    let estimate = price.usage_cost(&Usage {
        input: input_est,
        output: prepared.max_output_tokens,
        ..Usage::default()
    });

    let ancestors: Vec<String> = match &parent {
        None => Vec::new(),
        Some(p) => match st.ledger.ancestors(&org.id, &run_id, p).await {
            Ok(chain) => chain.as_ref().clone(),
            Err(e) => {
                tracing::error!(request_id, error = %e, "run ancestry lookup failed");
                if closed {
                    return unavailable();
                }
                degraded = true;
                vec![p.clone()]
            }
        },
    };

    let budgets = Budgets::effective(&org.budgets, &project.budgets);
    let now = Utc::now();
    let scopes = build_scopes(
        &Keys::new(&org.id),
        &budgets,
        &ScopeInput {
            project: &project.id,
            agent: &agent,
            run: &run_id,
            ancestors: &ancestors,
            run_budget_usd: run_budget_header,
            ymd: &now.format("%Y%m%d").to_string(),
            ym: &now.format("%Y%m").to_string(),
        },
    );

    let mut ctx = Ctx {
        request_id: request_id.clone(),
        org: org.id.clone(),
        project: project.id.clone(),
        agent,
        user,
        run_id: run_id.clone(),
        parent_run_id: parent,
        api,
        model: prepared.model.clone(),
        stream: prepared.stream,
        price,
        price_known,
        input_est,
        max_output: prepared.max_output_tokens,
        estimate,
        scopes,
        check_exceeded: Vec::new(),
        degraded,
        pre_upstream_us: 0,
    };
    if !price_known {
        tracing::warn!(request_id, model = %ctx.model, "unknown model price; reserving at the provider's most expensive rate");
    }

    let reservation_ttl = rt.cfg.server.upstream_timeout_secs + RESERVATION_GRACE_SECS;
    let mut runs = Vec::with_capacity(ancestors.len() + 1);
    runs.push(run_id.clone());
    runs.extend(ancestors);
    let reserved = st
        .ledger
        .reserve(
            &org.id,
            &request_id,
            estimate,
            &ctx.scopes,
            &runs,
            ledger::unix_now() + reservation_ttl as i64,
            reservation_ttl + RESERVATION_RECORD_EXTRA_SECS,
        )
        .await;
    match reserved {
        Ok(Reservation::Allowed { check_exceeded }) => ctx.check_exceeded = check_exceeded,
        Ok(Reservation::OverBudget {
            label,
            spent,
            reserved,
            limit,
        }) => {
            emit(&st, &ctx, &Outcome::blocked("blocked_budget", &label));
            let message = format!(
                "TokenLens budget exceeded for {label}: spent ${} + in flight ${} + this request (up to ${}) is over the ${} limit",
                usd(spent),
                usd(reserved),
                usd(estimate as i64),
                usd(limit)
            );
            return with_run_header(
                reject(
                    StatusCode::PAYMENT_REQUIRED,
                    "tokenlens_budget_exceeded",
                    &message,
                ),
                &run_id,
            );
        }
        Ok(Reservation::RunStopped {
            run_id: stopped,
            state,
        }) => {
            emit(
                &st,
                &ctx,
                &Outcome::blocked("blocked_run_stopped", &stopped),
            );
            let kind = if state == "killed" {
                "tokenlens_run_killed"
            } else {
                "tokenlens_run_paused"
            };
            let message = format!(
                "TokenLens: run {stopped} is {state}; resume it from the dashboard or admin API"
            );
            return with_run_header(reject(StatusCode::FORBIDDEN, kind, &message), &run_id);
        }
        Err(e) => {
            tracing::error!(request_id, error = %e, "budget reservation failed");
            if closed {
                return unavailable();
            }
            ctx.degraded = true;
        }
    }
    ctx.pre_upstream_us = started.elapsed().as_micros() as u64;

    let url = format!("{base}{}", api.path());
    let (reply_tx, reply_rx) = oneshot::channel();
    // The upstream call runs in its own task so spend is settled even if the client disconnects.
    tokio::spawn(forward(
        st.clone(),
        ctx,
        url,
        upstream_headers,
        prepared.body,
        reply_tx,
    ));
    match reply_rx.await {
        Ok(resp) => resp,
        Err(_) => reject(
            StatusCode::BAD_GATEWAY,
            "tokenlens_upstream_error",
            "proxy task ended unexpectedly",
        ),
    }
}

pub(crate) struct ScopeInput<'a> {
    pub(crate) project: &'a str,
    pub(crate) agent: &'a str,
    pub(crate) run: &'a str,
    pub(crate) ancestors: &'a [String],
    pub(crate) run_budget_usd: Option<f64>,
    pub(crate) ymd: &'a str,
    pub(crate) ym: &'a str,
}

pub(crate) fn build_scopes(keys: &Keys, b: &Budgets, input: &ScopeInput) -> Vec<Scope> {
    let run_limit = match (input.run_budget_usd, b.run) {
        (Some(requested), _) => Some(b.max_run_usd.map_or(requested, |max| requested.min(max))),
        (None, Some(policy)) => Some(policy.limit_usd),
        (None, None) => None,
    };
    // An explicit per-run budget from the caller is enforced unless the org set a run policy mode.
    let run_mode = match (b.run, input.run_budget_usd) {
        (Some(policy), _) => policy.mode,
        (None, Some(_)) => Mode::Enforce,
        (None, None) => Mode::Skip,
    };
    let mut scopes = vec![Scope {
        label: format!("run:{}", input.run),
        key: keys.run_budget(input.run),
        limit: run_limit.map_or(LimitSpec::Stored, |l| {
            LimitSpec::SetIfAbsent(config::usd_to_micros(l))
        }),
        mode: run_mode,
        ttl_secs: RUN_TTL_SECS,
    }];
    for ancestor in input.ancestors {
        scopes.push(Scope {
            label: format!("run:{ancestor}"),
            key: keys.run_budget(ancestor),
            limit: LimitSpec::Stored,
            mode: Mode::Skip,
            ttl_secs: RUN_TTL_SECS,
        });
    }
    let period = |label: String,
                  key: String,
                  policy: Option<config::BudgetPolicy>,
                  ttl_secs: u64| match policy {
        Some(p) => Scope {
            label,
            key,
            limit: LimitSpec::Set(config::usd_to_micros(p.limit_usd)),
            mode: p.mode,
            ttl_secs,
        },
        None => Scope {
            label,
            key,
            limit: LimitSpec::Unlimited,
            mode: Mode::Skip,
            ttl_secs,
        },
    };
    scopes.push(period(
        format!("agent_daily:{}/{}", input.project, input.agent),
        keys.agent_daily(input.project, input.agent, input.ymd),
        b.agent_daily,
        DAILY_TTL_SECS,
    ));
    scopes.push(period(
        format!("project_monthly:{}", input.project),
        keys.project_monthly(input.project, input.ym),
        b.project_monthly,
        MONTHLY_TTL_SECS,
    ));
    scopes.push(period(
        "org_monthly".into(),
        keys.org_monthly(input.ym),
        b.org_monthly,
        MONTHLY_TTL_SECS,
    ));
    scopes
}

async fn forward(
    st: Arc<AppState>,
    ctx: Ctx,
    url: String,
    headers: HeaderMap,
    body: Vec<u8>,
    reply: oneshot::Sender<Response>,
) {
    let sent_at = Instant::now();
    let resp = match st.http.post(&url).headers(headers).body(body).send().await {
        Ok(r) => r,
        Err(e) => {
            settle(&st, &ctx, 0).await;
            let message = if e.is_timeout() {
                "upstream provider timed out"
            } else {
                "could not reach upstream provider"
            };
            tracing::warn!(request_id = %ctx.request_id, error = %e, "{message}");
            emit(
                &st,
                &ctx,
                &Outcome {
                    decision: "upstream_error",
                    blocked_scope: None,
                    usage: None,
                    usage_complete: true,
                    actual: 0,
                    upstream_status: None,
                    upstream_ms: sent_at.elapsed().as_millis() as u64,
                    client_disconnected: false,
                },
            );
            let resp = error_response(
                ctx.api,
                StatusCode::BAD_GATEWAY,
                "tokenlens_upstream_error",
                message,
                &ctx.request_id,
            );
            let _ = reply.send(with_run_header(resp, &ctx.run_id));
            return;
        }
    };

    let status = resp.status();
    let is_sse = resp
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|ct| ct.starts_with("text/event-stream"));
    let mut out_headers = copy_upstream_headers(resp.headers());
    add_tl_headers(&mut out_headers, &ctx);

    if status.is_success() && is_sse {
        stream_through(st, ctx, resp, status, out_headers, reply, sent_at).await;
        return;
    }

    let bytes = match resp.bytes().await {
        Ok(b) => b,
        Err(e) => {
            // The provider may have billed for a response we failed to read, so charge the reservation.
            let actual = if status.is_success() { ctx.estimate } else { 0 };
            settle(&st, &ctx, actual).await;
            tracing::warn!(request_id = %ctx.request_id, error = %e, "failed reading upstream body");
            let resp = error_response(
                ctx.api,
                StatusCode::BAD_GATEWAY,
                "tokenlens_upstream_error",
                "upstream response was cut off",
                &ctx.request_id,
            );
            let _ = reply.send(with_run_header(resp, &ctx.run_id));
            return;
        }
    };

    let (actual, usage, complete) = if status.is_success() {
        let parsed: Option<Value> = serde_json::from_slice(&bytes).ok();
        match parsed
            .as_ref()
            .and_then(|v| providers::usage_from_body(ctx.api, v))
        {
            Some(u) => {
                if ctx.api == Api::OpenAIResponses
                    && let Some(id) = parsed.as_ref().and_then(providers::response_id)
                {
                    remember_context(&st, &ctx, &id, &u).await;
                }
                (ctx.price.usage_cost(&u), Some(u), true)
            }
            None => (ctx.estimate, None, false),
        }
    } else {
        (0, None, true)
    };
    settle(&st, &ctx, actual).await;
    emit(
        &st,
        &ctx,
        &Outcome {
            decision: if ctx.degraded {
                "allowed_degraded"
            } else {
                "allowed"
            },
            blocked_scope: None,
            usage,
            usage_complete: complete,
            actual,
            upstream_status: Some(status.as_u16()),
            upstream_ms: sent_at.elapsed().as_millis() as u64,
            client_disconnected: false,
        },
    );
    let mut response = Response::new(Body::from(bytes));
    *response.status_mut() = status;
    *response.headers_mut() = out_headers;
    let _ = reply.send(response);
}

async fn stream_through(
    st: Arc<AppState>,
    ctx: Ctx,
    resp: reqwest::Response,
    status: StatusCode,
    headers: HeaderMap,
    reply: oneshot::Sender<Response>,
    sent_at: Instant,
) {
    let (tx, rx) = mpsc::channel::<Result<Bytes, std::io::Error>>(64);
    let mut response = Response::new(Body::from_stream(ReceiverStream::new(rx)));
    *response.status_mut() = status;
    *response.headers_mut() = headers;
    let _ = reply.send(response);

    let mut tracker = StreamTracker::new(ctx.api);
    let mut upstream = resp.bytes_stream();
    let mut client_disconnected = false;
    while let Some(item) = upstream.next().await {
        match item {
            Ok(chunk) => {
                tracker.feed(&chunk);
                if tx.send(Ok(chunk)).await.is_err() {
                    client_disconnected = true;
                    break;
                }
            }
            Err(e) => {
                tracing::warn!(request_id = %ctx.request_id, error = %e, "upstream stream failed");
                let _ = tx.send(Err(std::io::Error::other(e))).await;
                break;
            }
        }
    }
    drop(tx);
    // Dropping the upstream body closes the connection, which stops generation after a client disconnect.
    drop(upstream);

    let result = tracker.finish();
    let usage = match result.usage {
        Some(u) if result.complete => u,
        Some(mut u) => {
            u.output = u
                .output
                .max(result.approx_output_tokens)
                .min(ctx.max_output);
            if u.input + u.cached_input + u.cache_write == 0 {
                u.input = ctx.input_est;
            }
            u
        }
        None => Usage {
            input: ctx.input_est,
            output: result.approx_output_tokens.min(ctx.max_output),
            ..Usage::default()
        },
    };
    if result.complete
        && ctx.api == Api::OpenAIResponses
        && let Some(id) = &result.response_id
    {
        remember_context(&st, &ctx, id, &usage).await;
    }
    let actual = ctx.price.usage_cost(&usage);
    settle(&st, &ctx, actual).await;
    emit(
        &st,
        &ctx,
        &Outcome {
            decision: if ctx.degraded {
                "allowed_degraded"
            } else {
                "allowed"
            },
            blocked_scope: None,
            usage: Some(usage),
            usage_complete: result.complete,
            actual,
            upstream_status: Some(status.as_u16()),
            upstream_ms: sent_at.elapsed().as_millis() as u64,
            client_disconnected,
        },
    );
}

async fn remember_context(st: &AppState, ctx: &Ctx, response_id: &str, u: &Usage) {
    if !config::valid_id(response_id) {
        return;
    }
    let tokens = u.input + u.cached_input + u.cache_write + u.output;
    if let Err(e) = st
        .ledger
        .remember_response_tokens(&ctx.org, response_id, tokens)
        .await
    {
        tracing::warn!(request_id = %ctx.request_id, error = %e, "could not store response context size");
    }
}

async fn settle(st: &AppState, ctx: &Ctx, actual: u64) {
    for attempt in 0..3u32 {
        match st
            .ledger
            .settle(&ctx.org, &ctx.request_id, actual, &ctx.scopes)
            .await
        {
            Ok(_) => return,
            Err(e) => {
                tracing::warn!(request_id = %ctx.request_id, attempt, error = %e, "settle failed");
                tokio::time::sleep(Duration::from_millis(50 << attempt)).await;
            }
        }
    }
    tracing::error!(
        request_id = %ctx.request_id,
        actual_micros = actual,
        "could not settle spend; the sweeper will release the reservation but this spend is unrecorded"
    );
}

fn emit(st: &AppState, ctx: &Ctx, o: &Outcome) {
    let event = json!({
        "event": "request",
        "ts": Utc::now().to_rfc3339(),
        "request_id": ctx.request_id,
        "org": ctx.org,
        "project": ctx.project,
        "agent": ctx.agent,
        "user": ctx.user,
        "run_id": ctx.run_id,
        "parent_run_id": ctx.parent_run_id,
        "provider": ctx.api.provider().name(),
        "model": ctx.model,
        "stream": ctx.stream,
        "decision": o.decision,
        "blocked_scope": o.blocked_scope,
        "check_exceeded": ctx.check_exceeded,
        "degraded": ctx.degraded,
        "price_known": ctx.price_known,
        "estimate_micros": ctx.estimate,
        "actual_micros": o.actual,
        "usage": o.usage,
        "usage_complete": o.usage_complete,
        "upstream_status": o.upstream_status,
        "pre_upstream_us": ctx.pre_upstream_us,
        "upstream_ms": o.upstream_ms,
        "client_disconnected": o.client_disconnected,
    });
    println!("{event}");
    // Dashboard activity is best effort: never block or fail a request for it.
    let _ = st.activity.try_send(event);
}

fn usd(micros: i64) -> String {
    format!("{:.4}", micros as f64 / 1_000_000.0)
}

pub fn client_token(headers: &HeaderMap) -> Option<&str> {
    if let Some(v) = headers.get(AUTHORIZATION).and_then(|v| v.to_str().ok())
        && let Some(t) = v
            .strip_prefix("Bearer ")
            .or_else(|| v.strip_prefix("bearer "))
    {
        return Some(t.trim()).filter(|t| !t.is_empty());
    }
    headers
        .get("x-api-key")
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|t| !t.is_empty())
}

fn header_id(headers: &HeaderMap, name: &str) -> Result<Option<String>, String> {
    let Some(v) = headers.get(name) else {
        return Ok(None);
    };
    let s = v
        .to_str()
        .map_err(|_| format!("{name} must be ASCII"))?
        .trim();
    if config::valid_id(s) {
        Ok(Some(s.to_string()))
    } else {
        Err(format!(
            "{name} must be 1-128 letters, digits, '.', '_' or '-'"
        ))
    }
}

fn error_response(
    api: Api,
    status: StatusCode,
    kind: &str,
    message: &str,
    request_id: &str,
) -> Response {
    let mut resp = (
        status,
        axum::Json(providers::error_body(api, kind, message)),
    )
        .into_response();
    if let Ok(v) = HeaderValue::from_str(request_id) {
        resp.headers_mut()
            .insert(HeaderName::from_static("x-tl-request-id"), v);
    }
    resp
}

fn with_run_header(mut resp: Response, run_id: &str) -> Response {
    if let Ok(v) = HeaderValue::from_str(run_id) {
        resp.headers_mut()
            .insert(HeaderName::from_static("x-tl-run-id"), v);
    }
    resp
}

const HOP_BY_HOP: [&str; 10] = [
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
    "content-length",
    "set-cookie",
];

fn copy_upstream_headers(src: &HeaderMap) -> HeaderMap {
    let mut h = HeaderMap::new();
    for (name, value) in src {
        if !HOP_BY_HOP.contains(&name.as_str()) {
            h.append(name.clone(), value.clone());
        }
    }
    h
}

fn add_tl_headers(h: &mut HeaderMap, ctx: &Ctx) {
    let mut set = |name: &'static str, value: &str| {
        if let Ok(v) = HeaderValue::from_str(value) {
            h.insert(HeaderName::from_static(name), v);
        }
    };
    set("x-tl-request-id", &ctx.request_id);
    set("x-tl-run-id", &ctx.run_id);
    if !ctx.check_exceeded.is_empty() {
        set(
            "x-tl-warning",
            &format!(
                "check-mode budget exceeded: {}",
                ctx.check_exceeded.join(", ")
            ),
        );
    }
    if ctx.degraded {
        set("x-tl-degraded", "true");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::BudgetPolicy;

    fn input<'a>(ancestors: &'a [String], header: Option<f64>) -> ScopeInput<'a> {
        ScopeInput {
            project: "bot",
            agent: "a1",
            run: "r1",
            ancestors,
            run_budget_usd: header,
            ymd: "20261018",
            ym: "202610",
        }
    }

    #[test]
    fn header_budget_is_capped_and_enforced_without_policy() {
        let b = Budgets {
            max_run_usd: Some(2.0),
            ..Budgets::default()
        };
        let s = build_scopes(&Keys::new("acme"), &b, &input(&[], Some(5.0)));
        assert_eq!(s[0].limit, LimitSpec::SetIfAbsent(2_000_000));
        assert_eq!(s[0].mode, Mode::Enforce);
        assert_eq!(s.len(), 4);
        assert_eq!(s[1].key, "tl:{acme}:b:agent:bot:a1:d:20261018");
    }

    #[test]
    fn policy_mode_wins_and_ancestors_use_stored_limits() {
        let b = Budgets {
            run: Some(BudgetPolicy {
                limit_usd: 1.0,
                mode: Mode::Check,
            }),
            org_monthly: Some(BudgetPolicy {
                limit_usd: 100.0,
                mode: Mode::Enforce,
            }),
            ..Budgets::default()
        };
        let ancestors = vec!["p1".to_string(), "p0".to_string()];
        let s = build_scopes(&Keys::new("acme"), &b, &input(&ancestors, None));
        assert_eq!(s[0].mode, Mode::Check);
        assert_eq!(s[1].limit, LimitSpec::Stored);
        assert_eq!(s[2].label, "run:p0");
        let org = s.last().unwrap();
        assert_eq!(
            (org.limit.clone(), org.mode),
            (LimitSpec::Set(100_000_000), Mode::Enforce)
        );
    }

    #[test]
    fn token_from_either_auth_header() {
        let mut h = HeaderMap::new();
        h.insert(AUTHORIZATION, HeaderValue::from_static("Bearer tl_abc"));
        assert_eq!(client_token(&h), Some("tl_abc"));
        let mut h = HeaderMap::new();
        h.insert("x-api-key", HeaderValue::from_static("tl_xyz"));
        assert_eq!(client_token(&h), Some("tl_xyz"));
        assert_eq!(client_token(&HeaderMap::new()), None);
    }
}
