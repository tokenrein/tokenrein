//! Usage from agents and SDKs that call model providers directly, with TokenLens out of the
//! request path:
//!
//! - `POST /v1/usage`: usage events reported after each call (SDK direct mode, any client).
//! - `POST /v1/check`: "may this run make another call?", asked before a call or agent step.
//! - `POST /v1/otel/v1/logs`: OpenTelemetry logs (OTLP over HTTP/JSON) from agents with
//!   built-in telemetry, such as Claude Code's `claude_code.api_request` events.
//! - `POST /v1/hooks/claude-code`: Claude Code HTTP hooks; blocks the next prompt or tool call
//!   when a run is paused, killed or over an enforced budget.
//!
//! All four authenticate with a TokenLens virtual key, like the proxy. Reported spend is
//! charged to the same budgets and shows up in the same dashboard. Enforcement is between calls
//! (the call that crosses a limit has already happened), unlike the proxy's pre-call reservation.

use std::sync::Arc;

use axum::Json;
use axum::extract::State;
use axum::http::header::CONTENT_TYPE;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use chrono::Utc;
use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::config::{self, Budgets, Resolved, usd_to_micros, valid_id, valid_user};
use crate::ledger::{Keys, Reservation, Scope};
use crate::providers::Usage;
use crate::proxy::{AppState, ScopeInput, build_scopes, client_token};

/// Most events accepted in one `/v1/usage` request.
const MAX_EVENTS: usize = 500;

struct Caller {
    rt: Arc<Resolved>,
    oi: usize,
    pi: usize,
}

impl Caller {
    fn org(&self) -> &config::OrgConfig {
        &self.rt.cfg.orgs[self.oi]
    }
    fn project(&self) -> &config::ProjectConfig {
        &self.org().projects[self.pi]
    }
    fn budgets(&self) -> Budgets {
        Budgets::effective(&self.org().budgets, &self.project().budgets)
    }
}

fn error(status: StatusCode, kind: &str, message: &str) -> Response {
    (
        status,
        Json(json!({ "error": { "type": kind, "message": message } })),
    )
        .into_response()
}

#[allow(clippy::result_large_err)]
fn caller(st: &AppState, headers: &HeaderMap) -> Result<Caller, Response> {
    let Some(token) = client_token(headers) else {
        return Err(error(
            StatusCode::UNAUTHORIZED,
            "tokenlens_unauthorized",
            "missing TokenLens key (send it as 'Authorization: Bearer' or 'x-api-key')",
        ));
    };
    let rt = st.rt();
    let Some(&(oi, pi)) = rt.keys.get(&config::sha256_hex(token)) else {
        return Err(error(
            StatusCode::UNAUTHORIZED,
            "tokenlens_unauthorized",
            "invalid TokenLens key",
        ));
    };
    Ok(Caller { rt, oi, pi })
}

/// Turn any string (a session UUID, an agent name) into a valid id.
fn to_id(s: &str) -> Option<String> {
    let cleaned: String = s
        .trim()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '-'
            }
        })
        .take(128)
        .collect();
    valid_id(&cleaned).then_some(cleaned)
}

fn header_str<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

/// Where a reported call belongs.
struct Who {
    run: String,
    parent: Option<String>,
    agent: String,
    user: Option<String>,
    source: String,
    run_budget_usd: Option<f64>,
}

async fn scopes_for(
    st: &AppState,
    c: &Caller,
    who: &Who,
) -> anyhow::Result<(Vec<Scope>, Vec<String>)> {
    let ancestors: Vec<String> = match &who.parent {
        None => Vec::new(),
        Some(p) => st
            .ledger
            .ancestors(&c.org().id, &who.run, p)
            .await?
            .as_ref()
            .clone(),
    };
    let now = Utc::now();
    let scopes = build_scopes(
        &Keys::new(&c.org().id),
        &c.budgets(),
        &ScopeInput {
            project: &c.project().id,
            agent: &who.agent,
            run: &who.run,
            ancestors: &ancestors,
            run_budget_usd: who.run_budget_usd,
            ymd: &now.format("%Y%m%d").to_string(),
            ym: &now.format("%Y%m").to_string(),
        },
    );
    let mut runs = vec![who.run.clone()];
    runs.extend(ancestors);
    Ok((scopes, runs))
}

fn unavailable(e: anyhow::Error) -> Response {
    tracing::error!(error = %e, "budget ledger call failed");
    crate::metrics::ledger_error();
    error(
        StatusCode::SERVICE_UNAVAILABLE,
        "tokenlens_unavailable",
        "budget ledger unavailable",
    )
}

// --- Reported usage ---

#[derive(Debug, Default, Deserialize)]
pub struct UsageEvent {
    pub run_id: Option<String>,
    pub parent_run_id: Option<String>,
    pub agent: Option<String>,
    pub user: Option<String>,
    /// "openai", "anthropic", "google", "groq", "ollama", ...
    pub provider: Option<String>,
    pub model: String,
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
    #[serde(default)]
    pub cache_read_tokens: u64,
    #[serde(default)]
    pub cache_write_tokens: u64,
    /// Cost the client computed; otherwise TokenLens prices the tokens.
    pub cost_usd: Option<f64>,
    pub duration_ms: Option<u64>,
    pub run_budget_usd: Option<f64>,
    pub request_id: Option<String>,
    pub source: Option<String>,
    /// Provider HTTP status, when the call failed.
    pub status: Option<u16>,
    pub stream: Option<bool>,
}

#[derive(Deserialize)]
pub struct UsageBody {
    #[serde(default)]
    events: Vec<UsageEvent>,
}

#[derive(Default)]
struct Recorded {
    /// Enforced limits this run has now used up: the next call will be refused.
    over: Vec<String>,
}

fn who_from_event(ev: &UsageEvent, default_source: &str) -> Result<Who, String> {
    let run = match &ev.run_id {
        Some(r) if valid_id(r) => r.clone(),
        Some(_) => return Err("run_id must be 1-128 letters, digits, '.', '_' or '-'".into()),
        None => format!("req_{}", uuid::Uuid::new_v4().simple()),
    };
    let parent = match &ev.parent_run_id {
        Some(p) if valid_id(p) && *p != run => Some(p.clone()),
        Some(_) => return Err("invalid parent_run_id".into()),
        None => None,
    };
    let agent = match &ev.agent {
        Some(a) if valid_id(a) => a.clone(),
        Some(_) => return Err("agent must be 1-128 letters, digits, '.', '_' or '-'".into()),
        None => "default".into(),
    };
    let user = match &ev.user {
        Some(u) if valid_user(u) => Some(u.clone()),
        Some(_) => {
            return Err("user must be 1-128 letters, digits, '.', '_', '-', '@' or '+'".into());
        }
        None => None,
    };
    if let Some(b) = ev.run_budget_usd
        && !(b.is_finite() && b > 0.0)
    {
        return Err("run_budget_usd must be a positive number of US dollars".into());
    }
    let source = ev
        .source
        .as_deref()
        .and_then(to_id)
        .unwrap_or_else(|| default_source.to_string());
    Ok(Who {
        run,
        parent,
        agent,
        user,
        source,
        run_budget_usd: ev.run_budget_usd,
    })
}

async fn record_usage(
    st: &AppState,
    c: &Caller,
    ev: &UsageEvent,
    who: &Who,
) -> anyhow::Result<Recorded> {
    let provider = ev
        .provider
        .as_deref()
        .map(|p| p.trim().to_ascii_lowercase())
        .unwrap_or_default();
    let usage = Usage {
        input: ev.input_tokens,
        cached_input: ev.cache_read_tokens,
        cache_write: ev.cache_write_tokens,
        output: ev.output_tokens,
    };
    let (price, price_known) = c.rt.prices.lookup_any(&provider, &ev.model);
    let (actual, price_known) = match ev.cost_usd {
        Some(usd) if usd.is_finite() && usd >= 0.0 => (usd_to_micros(usd), true),
        _ => (price.usage_cost(&usage), price_known),
    };
    let (scopes, _) = scopes_for(st, c, who).await?;
    let over = st.ledger.charge(&scopes, actual).await?;
    let check_exceeded: Vec<&String> = over
        .iter()
        .filter(|(_, m)| *m == config::Mode::Check)
        .map(|(l, _)| l)
        .collect();
    let enforced: Vec<String> = over
        .iter()
        .filter(|(_, m)| *m == config::Mode::Enforce)
        .map(|(l, _)| l.clone())
        .collect();
    let failed = ev.status.is_some_and(|s| s >= 400);
    let event = json!({
        "event": "request",
        "ts": Utc::now().to_rfc3339(),
        "request_id": ev.request_id.clone().unwrap_or_else(|| format!("rep_{}", uuid::Uuid::new_v4().simple())),
        "org": c.org().id,
        "project": c.project().id,
        "agent": who.agent,
        "user": who.user,
        "run_id": who.run,
        "parent_run_id": who.parent,
        "provider": if provider.is_empty() { Value::Null } else { json!(provider) },
        "model": ev.model,
        "stream": ev.stream.unwrap_or(false),
        "decision": if failed { "upstream_error" } else { "allowed" },
        "blocked_scope": Value::Null,
        "check_exceeded": check_exceeded,
        "degraded": false,
        "price_known": price_known,
        "estimate_micros": 0,
        "actual_micros": actual,
        "usage": usage,
        "usage_complete": true,
        "upstream_status": ev.status,
        "pre_upstream_us": 0,
        "upstream_ms": ev.duration_ms.unwrap_or(0),
        "client_disconnected": false,
        "source": who.source,
    });
    println!("{event}");
    crate::metrics::publish(&st.activity, crate::metrics::Path::Ingest, event);
    Ok(Recorded { over: enforced })
}

pub async fn usage(
    State(st): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<UsageBody>,
) -> Response {
    let c = match caller(&st, &headers) {
        Ok(c) => c,
        Err(r) => return r,
    };
    if body.events.len() > MAX_EVENTS {
        return error(
            StatusCode::PAYLOAD_TOO_LARGE,
            "tokenlens_invalid_request",
            "at most 500 events per request",
        );
    }
    let mut over = Vec::new();
    let mut rejected = Vec::new();
    for (i, ev) in body.events.iter().enumerate() {
        if ev.model.is_empty() || ev.model.len() > 200 {
            rejected.push(json!({ "index": i, "error": "model must be 1-200 characters" }));
            continue;
        }
        let who = match who_from_event(ev, "sdk") {
            Ok(w) => w,
            Err(e) => {
                rejected.push(json!({ "index": i, "error": e }));
                continue;
            }
        };
        match record_usage(&st, &c, ev, &who).await {
            Ok(r) => over.extend(r.over),
            Err(e) => return unavailable(e),
        }
    }
    over.sort();
    over.dedup();
    Json(json!({
        "accepted": body.events.len() - rejected.len(),
        "rejected": rejected,
        // Enforced limits now used up: further calls in these scopes will be refused.
        "over": over,
    }))
    .into_response()
}

// --- Budget check before a call or step ---

#[derive(Debug, Default, Deserialize)]
pub struct CheckBody {
    pub run_id: Option<String>,
    pub parent_run_id: Option<String>,
    pub agent: Option<String>,
    pub user: Option<String>,
    pub run_budget_usd: Option<f64>,
    pub provider: Option<String>,
    pub model: Option<String>,
    /// Upper bound of the coming call's output, to estimate its cost.
    pub max_output_tokens: Option<u64>,
    pub estimate_usd: Option<f64>,
    pub source: Option<String>,
}

struct Verdict {
    allowed: bool,
    reason: Option<String>,
    scope: Option<String>,
    run_state: Option<String>,
    check_exceeded: Vec<String>,
}

fn usd_text(micros: i64) -> String {
    format!("${:.4}", micros as f64 / 1_000_000.0)
}

async fn verdict(st: &AppState, c: &Caller, who: &Who, estimate: u64) -> anyhow::Result<Verdict> {
    let (scopes, runs) = scopes_for(st, c, who).await?;
    Ok(
        match st
            .ledger
            .check(&c.org().id, estimate, &scopes, &runs)
            .await?
        {
            Reservation::Allowed { check_exceeded } => Verdict {
                allowed: true,
                reason: None,
                scope: None,
                run_state: None,
                check_exceeded,
            },
            Reservation::OverBudget {
                label,
                spent,
                reserved,
                limit,
            } => Verdict {
                allowed: false,
                reason: Some(format!(
                    "TokenLens: the {label} budget is used up ({} spent{} of {}). Ask an admin to raise it in the TokenLens dashboard.",
                    usd_text(spent),
                    if reserved > 0 {
                        format!(" + {} in flight", usd_text(reserved))
                    } else {
                        String::new()
                    },
                    usd_text(limit)
                )),
                scope: Some(label),
                run_state: None,
                check_exceeded: Vec::new(),
            },
            Reservation::RunStopped { run_id, state } => Verdict {
                allowed: false,
                reason: Some(if state == "killed" {
                    format!("TokenLens: run {run_id} was stopped by an admin and cannot continue.")
                } else {
                    format!(
                        "TokenLens: run {run_id} is paused by an admin. Resume it in the TokenLens dashboard to continue."
                    )
                }),
                scope: Some(format!("run:{run_id}")),
                run_state: Some(state),
                check_exceeded: Vec::new(),
            },
        },
    )
}

fn blocked_event(c: &Caller, who: &Who, v: &Verdict, model: Option<&str>) -> Value {
    json!({
        "event": "request",
        "ts": Utc::now().to_rfc3339(),
        "request_id": format!("chk_{}", uuid::Uuid::new_v4().simple()),
        "org": c.org().id,
        "project": c.project().id,
        "agent": who.agent,
        "user": who.user,
        "run_id": who.run,
        "parent_run_id": who.parent,
        "provider": Value::Null,
        "model": model.unwrap_or("-"),
        "stream": false,
        "decision": if v.run_state.is_some() { "blocked_run_stopped" } else { "blocked_budget" },
        "blocked_scope": v.scope,
        "check_exceeded": [],
        "degraded": false,
        "price_known": true,
        "estimate_micros": 0,
        "actual_micros": 0,
        "usage": Value::Null,
        "usage_complete": true,
        "upstream_status": Value::Null,
        "pre_upstream_us": 0,
        "upstream_ms": 0,
        "client_disconnected": false,
        "source": who.source,
    })
}

pub async fn check(
    State(st): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<CheckBody>,
) -> Response {
    let c = match caller(&st, &headers) {
        Ok(c) => c,
        Err(r) => return r,
    };
    let ev = UsageEvent {
        run_id: body.run_id.clone(),
        parent_run_id: body.parent_run_id.clone(),
        agent: body.agent.clone(),
        user: body.user.clone(),
        run_budget_usd: body.run_budget_usd,
        source: body.source.clone(),
        ..UsageEvent::default()
    };
    let who = match who_from_event(&ev, "sdk") {
        Ok(w) => w,
        Err(e) => return error(StatusCode::BAD_REQUEST, "tokenlens_invalid_request", &e),
    };
    let estimate = match (body.estimate_usd, &body.model, body.max_output_tokens) {
        (Some(usd), _, _) if usd.is_finite() && usd >= 0.0 => usd_to_micros(usd),
        (_, Some(model), Some(max_out)) => {
            let (price, _) =
                c.rt.prices
                    .lookup_any(body.provider.as_deref().unwrap_or(""), model);
            price.usage_cost(&Usage {
                output: max_out,
                ..Usage::default()
            })
        }
        _ => 0,
    };
    let v = match verdict(&st, &c, &who, estimate).await {
        Ok(v) => v,
        Err(e) => return unavailable(e),
    };
    if let Err(e) = st
        .ledger
        .touch(
            &c.org().id,
            &c.project().id,
            &who.agent,
            who.user.as_deref(),
            &who.run,
            who.parent.as_deref(),
            &who.source,
            false,
        )
        .await
    {
        return unavailable(e);
    }
    if !v.allowed {
        crate::metrics::publish(
            &st.activity,
            crate::metrics::Path::Ingest,
            blocked_event(&c, &who, &v, body.model.as_deref()),
        );
    }
    Json(json!({
        "allowed": v.allowed,
        "reason": v.reason,
        "scope": v.scope,
        "run_state": v.run_state,
        "run_id": who.run,
        "check_exceeded": v.check_exceeded,
    }))
    .into_response()
}

// --- OpenTelemetry logs (OTLP/HTTP JSON) ---

/// Attribute value from OTLP JSON: `{"stringValue": ..}`, `{"intValue": "12"}`, `{"doubleValue": 1.5}`, ...
fn any_value(v: &Value) -> Option<Value> {
    let o = v.as_object()?;
    if let Some(s) = o.get("stringValue") {
        return Some(s.clone());
    }
    if let Some(i) = o.get("intValue") {
        return match i {
            Value::String(s) => s.parse::<i64>().ok().map(Value::from),
            other => Some(other.clone()),
        };
    }
    if let Some(d) = o.get("doubleValue") {
        return Some(d.clone());
    }
    if let Some(b) = o.get("boolValue") {
        return Some(b.clone());
    }
    None
}

fn attributes(list: Option<&Value>, into: &mut Map<String, Value>) {
    for kv in list.and_then(Value::as_array).into_iter().flatten() {
        if let (Some(k), Some(v)) = (
            kv.get("key").and_then(Value::as_str),
            kv.get("value").and_then(any_value),
        ) {
            into.insert(k.to_string(), v);
        }
    }
}

fn attr_str<'a>(a: &'a Map<String, Value>, k: &str) -> Option<&'a str> {
    a.get(k).and_then(Value::as_str).filter(|s| !s.is_empty())
}

fn attr_num(a: &Map<String, Value>, k: &str) -> Option<f64> {
    match a.get(k)? {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.parse().ok(),
        _ => None,
    }
}

/// Run ids for a Claude Code session: the session itself, and one child run for its sub-agents.
fn claude_code_runs(session: &str, subagent: bool) -> Option<(String, Option<String>)> {
    let root = to_id(&format!("cc-{session}"))?;
    if subagent {
        let child = to_id(&format!("cc-{session}-sub"))?;
        Some((child, Some(root)))
    } else {
        Some((root, None))
    }
}

pub async fn otel_logs(
    State(st): State<Arc<AppState>>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    let c = match caller(&st, &headers) {
        Ok(c) => c,
        Err(r) => return r,
    };
    let content_type = header_str(&headers, CONTENT_TYPE.as_str()).unwrap_or("");
    if !content_type.starts_with("application/json") {
        return error(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "tokenlens_invalid_request",
            "send OTLP logs as JSON: OTEL_EXPORTER_OTLP_LOGS_PROTOCOL=http/json",
        );
    }
    let doc: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(_) => {
            return error(
                StatusCode::BAD_REQUEST,
                "tokenlens_invalid_request",
                "invalid OTLP JSON",
            );
        }
    };
    let agent = header_str(&headers, "x-tl-agent")
        .and_then(to_id)
        .unwrap_or_else(|| "claude-code".into());
    let header_user = header_str(&headers, "x-tl-user").filter(|u| valid_user(u));
    let mut recorded = 0usize;
    for rl in doc
        .get("resourceLogs")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let mut resource = Map::new();
        attributes(rl.pointer("/resource/attributes"), &mut resource);
        for sl in rl
            .get("scopeLogs")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            for rec in sl
                .get("logRecords")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                let mut a = resource.clone();
                attributes(rec.get("attributes"), &mut a);
                let name = attr_str(&a, "event.name")
                    .or_else(|| rec.pointer("/body/stringValue").and_then(Value::as_str))
                    .unwrap_or("");
                let name = name.strip_prefix("claude_code.").unwrap_or(name);
                let Some(session) = attr_str(&a, "session.id") else {
                    continue;
                };
                let subagent = attr_str(&a, "query_source") == Some("subagent");
                let Some((run, parent)) = claude_code_runs(session, subagent) else {
                    continue;
                };
                let user = header_user
                    .map(str::to_string)
                    .or_else(|| attr_str(&a, "user.email").map(str::to_string))
                    .or_else(|| attr_str(&a, "user.account_id").map(str::to_string))
                    .filter(|u| valid_user(u));
                let who = Who {
                    run,
                    parent,
                    agent: agent.clone(),
                    user,
                    source: "claude-code".into(),
                    run_budget_usd: None,
                };
                match name {
                    "api_request" => {
                        let tokens = |k: &str| attr_num(&a, k).unwrap_or(0.0).max(0.0) as u64;
                        let cost_usd = attr_num(&a, "cost_usd_micros")
                            .map(|m| m / 1_000_000.0)
                            .or_else(|| attr_num(&a, "cost_usd"));
                        let ev = UsageEvent {
                            provider: Some("anthropic".into()),
                            model: attr_str(&a, "model").unwrap_or("unknown").to_string(),
                            input_tokens: tokens("input_tokens"),
                            output_tokens: tokens("output_tokens"),
                            cache_read_tokens: tokens("cache_read_tokens"),
                            cache_write_tokens: tokens("cache_creation_tokens"),
                            cost_usd,
                            duration_ms: attr_num(&a, "duration_ms").map(|d| d.max(0.0) as u64),
                            request_id: attr_str(&a, "request_id").map(str::to_string),
                            ..UsageEvent::default()
                        };
                        if let Err(e) = record_usage(&st, &c, &ev, &who).await {
                            return unavailable(e);
                        }
                        recorded += 1;
                    }
                    "user_prompt" | "tool_result" | "tool_decision" => {
                        if let Err(e) = st
                            .ledger
                            .touch(
                                &c.org().id,
                                &c.project().id,
                                &who.agent,
                                who.user.as_deref(),
                                &who.run,
                                who.parent.as_deref(),
                                &who.source,
                                false,
                            )
                            .await
                        {
                            return unavailable(e);
                        }
                    }
                    _ => {}
                }
            }
        }
    }
    tracing::debug!(recorded, "otlp logs ingested");
    Json(json!({})).into_response()
}

// --- Claude Code hooks ---

pub async fn claude_code_hook(
    State(st): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(input): Json<Value>,
) -> Response {
    let c = match caller(&st, &headers) {
        Ok(c) => c,
        Err(r) => return r,
    };
    let event = input
        .get("hook_event_name")
        .and_then(Value::as_str)
        .unwrap_or("");
    let Some(session) = input.get("session_id").and_then(Value::as_str) else {
        return Json(json!({})).into_response();
    };
    // Hooks fired inside a sub-agent carry its agent_id.
    let subagent = input.get("agent_id").and_then(Value::as_str).is_some()
        || event == "SubagentStart"
        || event == "SubagentStop";
    let Some((run, parent)) = claude_code_runs(session, subagent) else {
        return Json(json!({})).into_response();
    };
    let who = Who {
        run,
        parent,
        agent: header_str(&headers, "x-tl-agent")
            .and_then(to_id)
            .unwrap_or_else(|| "claude-code".into()),
        user: header_str(&headers, "x-tl-user")
            .filter(|u| valid_user(u))
            .map(str::to_string),
        source: "claude-code".into(),
        run_budget_usd: header_str(&headers, "x-tl-run-budget-usd")
            .and_then(|b| b.parse::<f64>().ok())
            .filter(|b| b.is_finite() && *b > 0.0),
    };
    if let Err(e) = st
        .ledger
        .touch(
            &c.org().id,
            &c.project().id,
            &who.agent,
            who.user.as_deref(),
            &who.run,
            who.parent.as_deref(),
            &who.source,
            event == "SessionEnd",
        )
        .await
    {
        return unavailable(e);
    }
    if !matches!(event, "UserPromptSubmit" | "PreToolUse" | "SubagentStart") {
        return Json(json!({})).into_response();
    }
    let v = match verdict(&st, &c, &who, 0).await {
        Ok(v) => v,
        // Never break someone's agent because the ledger is down; fail open.
        Err(e) => {
            tracing::error!(error = %e, "hook budget check failed; allowing");
            crate::metrics::ledger_error();
            return Json(json!({})).into_response();
        }
    };
    if v.allowed {
        return Json(json!({})).into_response();
    }
    crate::metrics::publish(
        &st.activity,
        crate::metrics::Path::Ingest,
        blocked_event(&c, &who, &v, None),
    );
    let reason = v
        .reason
        .unwrap_or_else(|| "TokenLens: budget used up".into());
    Json(match event {
        "PreToolUse" => json!({
            "hookSpecificOutput": {
                "hookEventName": "PreToolUse",
                "permissionDecision": "deny",
                "permissionDecisionReason": reason,
            }
        }),
        _ => json!({ "decision": "block", "reason": reason }),
    })
    .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_from_sessions() {
        assert_eq!(
            to_id("00893aaf-19fa-41d2-8238-13269b9b3ca0").as_deref(),
            Some("00893aaf-19fa-41d2-8238-13269b9b3ca0")
        );
        assert_eq!(to_id("a b/c").as_deref(), Some("a-b-c"));
        assert_eq!(to_id("  "), None);
        let (run, parent) = claude_code_runs("s1", true).unwrap();
        assert_eq!(
            (run.as_str(), parent.as_deref()),
            ("cc-s1-sub", Some("cc-s1"))
        );
    }

    #[test]
    fn otlp_values() {
        let mut m = Map::new();
        attributes(
            Some(&json!([
                {"key": "model", "value": {"stringValue": "claude-opus-5-5"}},
                {"key": "input_tokens", "value": {"intValue": "120"}},
                {"key": "cost_usd", "value": {"doubleValue": 0.25}},
                {"key": "x", "value": {"kvlistValue": {}}},
            ])),
            &mut m,
        );
        assert_eq!(attr_str(&m, "model"), Some("claude-opus-5-5"));
        assert_eq!(attr_num(&m, "input_tokens"), Some(120.0));
        assert_eq!(attr_num(&m, "cost_usd"), Some(0.25));
        assert!(!m.contains_key("x"));
    }
}
