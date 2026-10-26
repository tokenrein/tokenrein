//! Usage from agents and SDKs that call model providers directly, with Tokenrein out of the
//! request path:
//!
//! - `POST /v1/usage`: usage events reported after each call (SDK direct mode, any client).
//! - `POST /v1/check`: "may this run make another call?", asked before a call or agent step.
//! - `POST /v1/otel/v1/logs`: OpenTelemetry logs (OTLP over HTTP/JSON) from coding agents with
//!   built-in telemetry: Claude Code, OpenAI Codex CLI and Gemini CLI.
//! - `POST /v1/hooks/{claude-code,codex,gemini,cursor}`: coding agents' hooks; they block the
//!   next prompt or tool call when a run is paused, killed or over an enforced budget.
//!
//! All of them authenticate with a Tokenrein virtual key, like the proxy. Reported spend is
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

pub(crate) struct Caller {
    pub(crate) rt: Arc<Resolved>,
    pub(crate) oi: usize,
    pub(crate) pi: usize,
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
            "tokenrein_unauthorized",
            "missing Tokenrein key (send it as 'Authorization: Bearer' or 'x-api-key')",
        ));
    };
    let rt = st.rt();
    let Some(&(oi, pi)) = rt.keys.get(&config::sha256_hex(token)) else {
        return Err(error(
            StatusCode::UNAUTHORIZED,
            "tokenrein_unauthorized",
            "invalid Tokenrein key",
        ));
    };
    Ok(Caller { rt, oi, pi })
}

/// Turn any string (a session UUID, an agent name) into a valid id.
pub(crate) fn to_id(s: &str) -> Option<String> {
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
pub(crate) struct Who {
    pub(crate) run: String,
    pub(crate) parent: Option<String>,
    pub(crate) agent: String,
    pub(crate) user: Option<String>,
    pub(crate) source: String,
    pub(crate) run_budget_usd: Option<f64>,
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
        "tokenrein_unavailable",
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
    /// Cost the client computed; otherwise Tokenrein prices the tokens.
    pub cost_usd: Option<f64>,
    pub duration_ms: Option<u64>,
    pub run_budget_usd: Option<f64>,
    pub request_id: Option<String>,
    pub source: Option<String>,
    /// Provider HTTP status, when the call failed.
    pub status: Option<u16>,
    pub stream: Option<bool>,
    /// When the call happened, for usage imported after the fact; otherwise now.
    #[serde(skip)]
    pub ts: Option<String>,
}

#[derive(Deserialize)]
pub struct UsageBody {
    #[serde(default)]
    events: Vec<UsageEvent>,
}

#[derive(Default)]
pub(crate) struct Recorded {
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

pub(crate) async fn record_usage(
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
        .filter(|(_, m)| matches!(m, config::Mode::Enforce | config::Mode::Ask))
        .map(|(l, _)| l.clone())
        .collect();
    // A reported call took a run past an Ask-mode budget: it waits for an approver.
    for (label, _) in over.iter().filter(|(_, m)| *m == config::Mode::Ask) {
        let Some(budget_run) = label.strip_prefix("run:") else {
            continue;
        };
        let b = st
            .ledger
            .budget(&crate::ledger::Keys::new(&c.org().id).run_budget(budget_run))
            .await
            .unwrap_or_default();
        let num = |f: &str| b.get(f).and_then(|v| v.parse::<f64>().ok()).unwrap_or(0.0) as i64;
        let request = crate::approvals::Request {
            org: &c.org().id,
            run: budget_run,
            project: &c.project().id,
            agent: &who.agent,
            reason: crate::approvals::Reason::Budget {
                scope: label,
                spent: num("spent"),
                limit: num("limit"),
            },
        };
        if let Err(e) = crate::approvals::request(st, request).await {
            tracing::error!(error = %e, "could not request approval");
        }
    }
    let policy = crate::loops::policy_for(&c.rt, &c.org().id, &c.project().id);
    if policy.is_some() {
        let seen = crate::loops::Seen {
            org: &c.org().id,
            project: &c.project().id,
            agent: &who.agent,
            run: &who.run,
            fingerprint: None,
            call: true,
            cost: actual,
        };
        if let Err(e) = crate::loops::observe(st, policy, seen).await {
            tracing::warn!(error = %e, "loop detection failed");
        }
    }
    let failed = ev.status.is_some_and(|s| s >= 400);
    let event = json!({
        "event": "request",
        "ts": ev.ts.clone().unwrap_or_else(|| Utc::now().to_rfc3339()),
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
            "tokenrein_invalid_request",
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
    /// Hash of the coming request (model and last message), for loop detection. The SDKs compute
    /// it locally; the content never leaves the caller.
    pub fingerprint: Option<String>,
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
                limit,
                mode: config::Mode::Ask,
                ..
            } if label.starts_with("run:") => {
                let budget_run = &label["run:".len()..];
                let request = crate::approvals::Request {
                    org: &c.org().id,
                    run: budget_run,
                    project: &c.project().id,
                    agent: &who.agent,
                    reason: crate::approvals::Reason::Budget {
                        scope: &label,
                        spent,
                        limit,
                    },
                };
                crate::approvals::request(st, request).await?;
                Verdict {
                    allowed: false,
                    reason: Some(format!(
                        "Tokenrein: run {budget_run} reached its {} budget and is waiting for an approver in the Tokenrein dashboard.",
                        usd_text(limit)
                    )),
                    scope: Some(label.clone()),
                    run_state: Some("paused".into()),
                    check_exceeded: Vec::new(),
                }
            }
            Reservation::OverBudget {
                label,
                spent,
                reserved,
                limit,
                ..
            } => Verdict {
                allowed: false,
                reason: Some(format!(
                    "Tokenrein: the {label} budget is used up ({} spent{} of {}). Ask an admin to raise it in the Tokenrein dashboard.",
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
                    format!("Tokenrein: run {run_id} was stopped by an admin and cannot continue.")
                } else {
                    format!(
                        "Tokenrein: run {run_id} is paused by an admin. Resume it in the Tokenrein dashboard to continue."
                    )
                }),
                scope: Some(format!("run:{run_id}")),
                run_state: Some(state),
                check_exceeded: Vec::new(),
            },
        },
    )
}

/// Count a coming request or tool call for loop detection; a refusal when it tripped the run's
/// loop guard and the run must stop. Fails open.
async fn loop_stop(
    st: &AppState,
    c: &Caller,
    who: &Who,
    fingerprint: Option<&str>,
) -> Option<Verdict> {
    let policy = crate::loops::policy_for(&c.rt, &c.org().id, &c.project().id);
    policy?;
    let seen = crate::loops::Seen {
        org: &c.org().id,
        project: &c.project().id,
        agent: &who.agent,
        run: &who.run,
        fingerprint,
        call: false,
        cost: 0,
    };
    let action = match crate::loops::observe(st, policy, seen).await {
        Ok(a) => a?,
        Err(e) => {
            tracing::warn!(error = %e, "loop detection failed; allowing");
            return None;
        }
    };
    let killed = action == config::LoopAction::Kill;
    Some(Verdict {
        allowed: false,
        reason: Some(if killed {
            format!(
                "Tokenrein: run {} looks like a runaway loop and was stopped.",
                who.run
            )
        } else {
            format!(
                "Tokenrein: run {} looks like a runaway loop and is paused until an approver lets it continue.",
                who.run
            )
        }),
        scope: Some(format!("run:{}", who.run)),
        run_state: Some(if killed { "killed" } else { "paused" }.into()),
        check_exceeded: Vec::new(),
    })
}

/// The tool call a hook is about to allow, hashed for loop detection.
fn tool_fingerprint(input: &Value) -> Option<String> {
    let name = input.get("tool_name").and_then(Value::as_str).unwrap_or("");
    let args = input
        .get("tool_input")
        .or_else(|| input.get("command"))
        .map(Value::to_string)
        .unwrap_or_default();
    (!name.is_empty() || !args.is_empty()).then(|| crate::loops::fingerprint(&[name, &args]))
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
        Err(e) => return error(StatusCode::BAD_REQUEST, "tokenrein_invalid_request", &e),
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
    let fingerprint = body
        .fingerprint
        .as_deref()
        .filter(|f| crate::loops::valid_fingerprint(f));
    let v = match loop_stop(&st, &c, &who, fingerprint).await {
        Some(v) => v,
        None => match verdict(&st, &c, &who, estimate).await {
            Ok(v) => v,
            Err(e) => return unavailable(e),
        },
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

// --- Coding agents: telemetry and hooks ---

/// A coding agent that reports through its own telemetry and hooks, with Tokenrein out of its
/// request path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tool {
    ClaudeCode,
    Codex,
    Gemini,
    Cursor,
}

impl Tool {
    /// The default agent name and the `source` of its events.
    pub fn name(self) -> &'static str {
        match self {
            Tool::ClaudeCode => "claude-code",
            Tool::Codex => "codex",
            Tool::Gemini => "gemini-cli",
            Tool::Cursor => "cursor",
        }
    }

    /// Run ids are `<prefix>-<session>`.
    fn run_prefix(self) -> &'static str {
        match self {
            Tool::ClaudeCode => "cc",
            Tool::Codex => "codex",
            Tool::Gemini => "gemini",
            Tool::Cursor => "cursor",
        }
    }
}

/// Run ids for an agent session: the session itself, and one child run for its sub-agents.
pub(crate) fn session_runs(
    tool: Tool,
    session: &str,
    subagent: bool,
) -> Option<(String, Option<String>)> {
    let root = to_id(&format!("{}-{session}", tool.run_prefix()))?;
    if subagent {
        let child = to_id(&format!("{root}-sub"))?;
        Some((child, Some(root)))
    } else {
        Some((root, None))
    }
}

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

fn attr_tokens(a: &Map<String, Value>, k: &str) -> u64 {
    attr_num(a, k).unwrap_or(0.0).max(0.0) as u64
}

/// Which agent sent a log record: from the event name's prefix, else the resource's
/// `service.name`. Records from anything else are read as Claude Code's, as before.
fn otel_tool(event: &str, service: Option<&str>) -> Tool {
    if event.starts_with("codex.") {
        Tool::Codex
    } else if event.starts_with("gemini_cli.") {
        Tool::Gemini
    } else if event.starts_with("claude_code.") {
        Tool::ClaudeCode
    } else {
        match service {
            Some(s) if s.starts_with("codex") => Tool::Codex,
            Some("gemini-cli") => Tool::Gemini,
            _ => Tool::ClaudeCode,
        }
    }
}

/// One log record, read: whose session it belongs to and what it reports.
struct Record {
    session: String,
    subagent: bool,
    user: Option<String>,
    /// A model call; `None` for records that only show the session is active.
    usage: Option<UsageEvent>,
}

/// Read a log record from a coding agent; `None` for records Tokenrein does not use.
/// See docs/connectors.md for the fields and where they are documented.
fn read_record(tool: Tool, event: &str, a: &Map<String, Value>) -> Option<Record> {
    let user = |keys: &[&str]| keys.iter().find_map(|k| attr_str(a, k)).map(str::to_string);
    let model = || attr_str(a, "model").unwrap_or("unknown").to_string();
    match tool {
        Tool::ClaudeCode => {
            let name = event.strip_prefix("claude_code.").unwrap_or(event);
            let usage = match name {
                "api_request" => Some(UsageEvent {
                    provider: Some("anthropic".into()),
                    model: model(),
                    input_tokens: attr_tokens(a, "input_tokens"),
                    output_tokens: attr_tokens(a, "output_tokens"),
                    cache_read_tokens: attr_tokens(a, "cache_read_tokens"),
                    cache_write_tokens: attr_tokens(a, "cache_creation_tokens"),
                    cost_usd: attr_num(a, "cost_usd_micros")
                        .map(|m| m / 1_000_000.0)
                        .or_else(|| attr_num(a, "cost_usd")),
                    duration_ms: attr_num(a, "duration_ms").map(|d| d.max(0.0) as u64),
                    request_id: attr_str(a, "request_id").map(str::to_string),
                    ..UsageEvent::default()
                }),
                "user_prompt" | "tool_result" | "tool_decision" => None,
                _ => return None,
            };
            Some(Record {
                session: attr_str(a, "session.id")?.to_string(),
                subagent: attr_str(a, "query_source") == Some("subagent"),
                user: user(&["user.email", "user.account_id"]),
                usage,
            })
        }
        Tool::Codex => {
            let name = event.strip_prefix("codex.").unwrap_or(event);
            let usage = match name {
                // Token counts come with the stream's `response.completed` event.
                "sse_event"
                    if attr_str(a, "event.kind") == Some("response.completed")
                        && a.contains_key("input_token_count") =>
                {
                    // OpenAI counts cached input inside the input tokens.
                    let input = attr_tokens(a, "input_token_count");
                    let cached = attr_tokens(a, "cached_token_count").min(input);
                    Some(UsageEvent {
                        provider: Some("openai".into()),
                        model: model(),
                        input_tokens: input - cached,
                        output_tokens: attr_tokens(a, "output_token_count"),
                        cache_read_tokens: cached,
                        cache_write_tokens: attr_tokens(a, "cache_write_token_count"),
                        ..UsageEvent::default()
                    })
                }
                "conversation_starts" | "user_prompt" | "tool_decision" | "tool_result" => None,
                _ => return None,
            };
            Some(Record {
                session: attr_str(a, "conversation.id")?.to_string(),
                subagent: false,
                user: user(&["user.email", "user.account_id"]),
                usage,
            })
        }
        Tool::Gemini => {
            let name = event.strip_prefix("gemini_cli.").unwrap_or(event);
            let usage = match name {
                "api_response" => {
                    // Gemini counts cached content inside the prompt tokens, and bills
                    // thinking tokens as output.
                    let input = attr_tokens(a, "input_token_count");
                    let cached = attr_tokens(a, "cached_content_token_count").min(input);
                    Some(UsageEvent {
                        provider: Some("google".into()),
                        model: model(),
                        input_tokens: input - cached,
                        output_tokens: attr_tokens(a, "output_token_count")
                            + attr_tokens(a, "thoughts_token_count"),
                        cache_read_tokens: cached,
                        duration_ms: attr_num(a, "duration_ms").map(|d| d.max(0.0) as u64),
                        status: attr_num(a, "status_code")
                            .map(|s| s as u16)
                            .filter(|s| *s >= 400),
                        ..UsageEvent::default()
                    })
                }
                "user_prompt" | "tool_call" | "agent.start" => None,
                _ => return None,
            };
            Some(Record {
                session: attr_str(a, "session.id")?.to_string(),
                subagent: attr_str(a, "role") == Some("subagent"),
                user: user(&["user.email"]),
                usage,
            })
        }
        // Cursor has no telemetry export; its usage comes from the Admin API import.
        Tool::Cursor => None,
    }
}

/// OTLP logs from Claude Code, OpenAI Codex CLI and Gemini CLI.
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
            "tokenrein_invalid_request",
            "send OTLP logs as JSON (OTLP/HTTP with protocol http/json)",
        );
    }
    let doc: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(_) => {
            return error(
                StatusCode::BAD_REQUEST,
                "tokenrein_invalid_request",
                "invalid OTLP JSON",
            );
        }
    };
    let header_agent = header_str(&headers, "x-tr-agent").and_then(to_id);
    let header_user = header_str(&headers, "x-tr-user").filter(|u| valid_user(u));
    let mut recorded = 0usize;
    for rl in doc
        .get("resourceLogs")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let mut resource = Map::new();
        attributes(rl.pointer("/resource/attributes"), &mut resource);
        let service = attr_str(&resource, "service.name").map(str::to_string);
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
                let event = attr_str(&a, "event.name")
                    .or_else(|| rec.pointer("/body/stringValue").and_then(Value::as_str))
                    .unwrap_or("");
                let tool = otel_tool(event, service.as_deref());
                let Some(r) = read_record(tool, event, &a) else {
                    continue;
                };
                let Some((run, parent)) = session_runs(tool, &r.session, r.subagent) else {
                    continue;
                };
                let who = Who {
                    run,
                    parent,
                    agent: header_agent
                        .clone()
                        .unwrap_or_else(|| tool.name().to_string()),
                    user: header_user
                        .map(str::to_string)
                        .or(r.user)
                        .filter(|u| valid_user(u)),
                    source: tool.name().into(),
                    run_budget_usd: None,
                };
                let result = match &r.usage {
                    Some(ev) => record_usage(&st, &c, ev, &who).await.map(|_| recorded += 1),
                    None => {
                        st.ledger
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
                    }
                };
                if let Err(e) = result {
                    return unavailable(e);
                }
            }
        }
    }
    tracing::debug!(recorded, "otlp logs ingested");
    Json(json!({})).into_response()
}

/// OTLP traces and metrics: accepted and dropped. Gemini CLI exports them next to its logs when
/// it sends OTLP over HTTP; Tokenrein only uses the logs.
pub async fn otel_discard(State(st): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    match caller(&st, &headers) {
        Ok(_) => Json(json!({})).into_response(),
        Err(r) => r,
    }
}

/// What a hook call asks Tokenrein to allow.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Gate {
    /// A prompt is about to be sent.
    Prompt,
    /// A tool call (shell command, file edit, MCP tool, ...) is about to run.
    Tool,
    /// A sub-agent is about to start.
    Subagent,
    /// A model call is about to be made.
    Model,
}

/// One hook call, read.
struct HookCall<'a> {
    event: &'a str,
    session: &'a str,
    subagent: bool,
    gate: Option<Gate>,
    ended: bool,
    user: Option<&'a str>,
}

/// See docs/connectors.md for each tool's hook events and fields.
fn read_hook(tool: Tool, input: &Value) -> Option<HookCall<'_>> {
    let s = |k: &str| {
        input
            .get(k)
            .and_then(Value::as_str)
            .filter(|v| !v.is_empty())
    };
    let event = s("hook_event_name").unwrap_or("");
    let (gate, ended) = match (tool, event) {
        (Tool::ClaudeCode | Tool::Codex, "UserPromptSubmit") => (Some(Gate::Prompt), false),
        (Tool::ClaudeCode | Tool::Codex, "PreToolUse") => (Some(Gate::Tool), false),
        // Codex can't stop a sub-agent from a hook; its tool calls are still checked.
        (Tool::ClaudeCode, "SubagentStart") => (Some(Gate::Subagent), false),
        (Tool::ClaudeCode | Tool::Codex | Tool::Gemini, "SessionEnd") => (None, true),
        (Tool::Gemini, "BeforeAgent") => (Some(Gate::Prompt), false),
        (Tool::Gemini, "BeforeTool") => (Some(Gate::Tool), false),
        (Tool::Gemini, "BeforeModel") => (Some(Gate::Model), false),
        (Tool::Cursor, "beforeSubmitPrompt") => (Some(Gate::Prompt), false),
        (Tool::Cursor, "preToolUse" | "beforeShellExecution" | "beforeMCPExecution") => {
            (Some(Gate::Tool), false)
        }
        (Tool::Cursor, "subagentStart") => (Some(Gate::Subagent), false),
        (Tool::Cursor, "sessionEnd") => (None, true),
        _ => (None, false),
    };
    let (session, subagent) = match tool {
        // Hooks fired inside a Claude Code sub-agent carry its agent_id.
        Tool::ClaudeCode => (
            s("session_id")?,
            s("agent_id").is_some() || matches!(event, "SubagentStart" | "SubagentStop"),
        ),
        // Codex sub-agent hooks carry the parent session's id.
        Tool::Codex | Tool::Gemini => (s("session_id")?, false),
        // A sub-agent about to start counts against the conversation that starts it.
        Tool::Cursor => (
            s("conversation_id")
                .or_else(|| s("parent_conversation_id"))
                .or_else(|| s("session_id"))?,
            false,
        ),
    };
    Some(HookCall {
        event,
        session,
        subagent,
        gate,
        ended,
        user: if tool == Tool::Cursor {
            s("user_email")
        } else {
            None
        },
    })
}

/// The hook's answer when Tokenrein has no objection.
fn hook_allow(tool: Tool, gate: Option<Gate>) -> Value {
    match (tool, gate) {
        (Tool::Cursor, Some(Gate::Prompt)) => json!({ "continue": true }),
        (Tool::Cursor, Some(_)) => json!({ "permission": "allow" }),
        _ => json!({}),
    }
}

/// The hook's answer that stops the prompt, tool call, sub-agent or model call, in each
/// tool's own format.
fn hook_block(tool: Tool, gate: Gate, event: &str, reason: &str) -> Value {
    match (tool, gate) {
        (Tool::ClaudeCode | Tool::Codex, Gate::Tool) => json!({
            "hookSpecificOutput": {
                "hookEventName": event,
                "permissionDecision": "deny",
                "permissionDecisionReason": reason,
            }
        }),
        (Tool::ClaudeCode | Tool::Codex, _) => json!({ "decision": "block", "reason": reason }),
        (Tool::Gemini, _) => json!({ "decision": "deny", "reason": reason }),
        (Tool::Cursor, Gate::Prompt) => json!({ "continue": false, "user_message": reason }),
        (Tool::Cursor, _) => json!({
            "permission": "deny",
            "user_message": reason,
            "agent_message": reason,
        }),
    }
}

/// A coding agent's hook: records that its session is active, and stops the next prompt,
/// tool call, sub-agent or model call when the run is paused or killed or an enforced budget is
/// used up. Fails open: a ledger outage never stops anyone's agent.
async fn agent_hook(st: &AppState, headers: &HeaderMap, input: &Value, tool: Tool) -> Response {
    let c = match caller(st, headers) {
        Ok(c) => c,
        Err(r) => return r,
    };
    let Some(call) = read_hook(tool, input) else {
        return Json(json!({})).into_response();
    };
    let allow = Json(hook_allow(tool, call.gate)).into_response();
    let Some((run, parent)) = session_runs(tool, call.session, call.subagent) else {
        return allow;
    };
    let who = Who {
        run,
        parent,
        agent: header_str(headers, "x-tr-agent")
            .and_then(to_id)
            .unwrap_or_else(|| tool.name().into()),
        user: header_str(headers, "x-tr-user")
            .or(call.user)
            .filter(|u| valid_user(u))
            .map(str::to_string),
        source: tool.name().into(),
        run_budget_usd: header_str(headers, "x-tr-run-budget-usd")
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
            call.ended,
        )
        .await
    {
        if call.gate.is_some() {
            tracing::error!(error = %e, "hook could not record activity; allowing");
            return allow;
        }
        return unavailable(e);
    }
    let Some(gate) = call.gate else {
        return allow;
    };
    let fingerprint = if gate == Gate::Tool {
        tool_fingerprint(input)
    } else {
        None
    };
    let stopped = match fingerprint {
        Some(fp) => loop_stop(st, &c, &who, Some(&fp)).await,
        None => None,
    };
    let v = match stopped {
        Some(v) => v,
        None => match verdict(st, &c, &who, 0).await {
            Ok(v) => v,
            Err(e) => {
                tracing::error!(error = %e, "hook budget check failed; allowing");
                crate::metrics::ledger_error();
                return allow;
            }
        },
    };
    if v.allowed {
        return allow;
    }
    crate::metrics::publish(
        &st.activity,
        crate::metrics::Path::Ingest,
        blocked_event(&c, &who, &v, None),
    );
    let reason = v
        .reason
        .unwrap_or_else(|| "Tokenrein: budget used up".into());
    Json(hook_block(tool, gate, call.event, &reason)).into_response()
}

/// Claude Code HTTP hooks.
pub async fn claude_code_hook(
    State(st): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(input): Json<Value>,
) -> Response {
    agent_hook(&st, &headers, &input, Tool::ClaudeCode).await
}

/// OpenAI Codex CLI command hooks, forwarded by the Tokenrein hook script.
pub async fn codex_hook(
    State(st): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(input): Json<Value>,
) -> Response {
    agent_hook(&st, &headers, &input, Tool::Codex).await
}

/// Gemini CLI command hooks, forwarded by the Tokenrein hook script.
pub async fn gemini_hook(
    State(st): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(input): Json<Value>,
) -> Response {
    agent_hook(&st, &headers, &input, Tool::Gemini).await
}

/// Cursor command hooks, forwarded by the Tokenrein hook script.
pub async fn cursor_hook(
    State(st): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(input): Json<Value>,
) -> Response {
    agent_hook(&st, &headers, &input, Tool::Cursor).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn attrs(list: Value) -> Map<String, Value> {
        let mut m = Map::new();
        attributes(Some(&list), &mut m);
        m
    }

    #[test]
    fn ids_from_sessions() {
        assert_eq!(
            to_id("00893aaf-19fa-41d2-8238-13269b9b3ca0").as_deref(),
            Some("00893aaf-19fa-41d2-8238-13269b9b3ca0")
        );
        assert_eq!(to_id("a b/c").as_deref(), Some("a-b-c"));
        assert_eq!(to_id("  "), None);
        let (run, parent) = session_runs(Tool::ClaudeCode, "s1", true).unwrap();
        assert_eq!(
            (run.as_str(), parent.as_deref()),
            ("cc-s1-sub", Some("cc-s1"))
        );
        let (run, _) = session_runs(Tool::Codex, "thr_1", false).unwrap();
        assert_eq!(run, "codex-thr_1");
        let (run, _) = session_runs(Tool::Gemini, "g 1", false).unwrap();
        assert_eq!(run, "gemini-g-1");
    }

    #[test]
    fn otlp_values() {
        let m = attrs(json!([
            {"key": "model", "value": {"stringValue": "claude-opus-5-5"}},
            {"key": "input_tokens", "value": {"intValue": "120"}},
            {"key": "cost_usd", "value": {"doubleValue": 0.25}},
            {"key": "x", "value": {"kvlistValue": {}}},
        ]));
        assert_eq!(attr_str(&m, "model"), Some("claude-opus-5-5"));
        assert_eq!(attr_num(&m, "input_tokens"), Some(120.0));
        assert_eq!(attr_num(&m, "cost_usd"), Some(0.25));
        assert!(!m.contains_key("x"));
    }

    #[test]
    fn tells_agents_apart() {
        assert_eq!(otel_tool("codex.sse_event", None), Tool::Codex);
        assert_eq!(otel_tool("gemini_cli.api_response", None), Tool::Gemini);
        assert_eq!(
            otel_tool("api_request", Some("claude-code")),
            Tool::ClaudeCode
        );
        assert_eq!(otel_tool("sse_event", Some("codex_cli_rs")), Tool::Codex);
        assert_eq!(otel_tool("api_response", Some("gemini-cli")), Tool::Gemini);
        assert_eq!(otel_tool("api_request", None), Tool::ClaudeCode);
    }

    #[test]
    fn codex_usage_excludes_cached_input() {
        let a = attrs(json!([
            {"key": "event.kind", "value": {"stringValue": "response.completed"}},
            {"key": "conversation.id", "value": {"stringValue": "thr_1"}},
            {"key": "model", "value": {"stringValue": "gpt-5.5"}},
            {"key": "input_token_count", "value": {"stringValue": "1000"}},
            {"key": "cached_token_count", "value": {"intValue": "800"}},
            {"key": "output_token_count", "value": {"stringValue": "50"}},
        ]));
        let r = read_record(Tool::Codex, "codex.sse_event", &a).unwrap();
        let u = r.usage.unwrap();
        assert_eq!(
            (u.input_tokens, u.cache_read_tokens, u.output_tokens),
            (200, 800, 50)
        );
        // Other stream events don't carry usage and aren't used.
        let a = attrs(json!([
            {"key": "event.kind", "value": {"stringValue": "response.output_item.done"}},
            {"key": "conversation.id", "value": {"stringValue": "thr_1"}},
        ]));
        assert!(read_record(Tool::Codex, "codex.sse_event", &a).is_none());
    }

    #[test]
    fn gemini_usage_bills_thoughts_as_output() {
        let a = attrs(json!([
            {"key": "session.id", "value": {"stringValue": "s"}},
            {"key": "input_token_count", "value": {"intValue": 1000}},
            {"key": "cached_content_token_count", "value": {"intValue": 400}},
            {"key": "output_token_count", "value": {"intValue": 30}},
            {"key": "thoughts_token_count", "value": {"intValue": 70}},
            {"key": "role", "value": {"stringValue": "subagent"}},
        ]));
        let r = read_record(Tool::Gemini, "gemini_cli.api_response", &a).unwrap();
        assert!(r.subagent);
        let u = r.usage.unwrap();
        assert_eq!(
            (u.input_tokens, u.cache_read_tokens, u.output_tokens),
            (600, 400, 100)
        );
        // The semantic-convention copy of the same call is not counted twice.
        assert!(
            read_record(
                Tool::Gemini,
                "gen_ai.client.inference.operation.details",
                &a
            )
            .is_none()
        );
    }

    #[test]
    fn hook_answers_in_each_tools_format() {
        let r = "over";
        assert_eq!(
            hook_block(Tool::Codex, Gate::Tool, "PreToolUse", r)["hookSpecificOutput"]["permissionDecision"],
            "deny"
        );
        assert_eq!(
            hook_block(Tool::Codex, Gate::Prompt, "UserPromptSubmit", r)["decision"],
            "block"
        );
        assert_eq!(
            hook_block(Tool::Gemini, Gate::Model, "BeforeModel", r)["decision"],
            "deny"
        );
        assert_eq!(
            hook_block(Tool::Cursor, Gate::Prompt, "beforeSubmitPrompt", r)["continue"],
            false
        );
        assert_eq!(
            hook_block(Tool::Cursor, Gate::Tool, "preToolUse", r)["permission"],
            "deny"
        );
        assert_eq!(
            hook_allow(Tool::Cursor, Some(Gate::Subagent))["permission"],
            "allow"
        );
        assert_eq!(hook_allow(Tool::Gemini, Some(Gate::Tool)), json!({}));
        let input = json!({"hook_event_name": "subagentStart", "parent_conversation_id": "c1"});
        let call = read_hook(Tool::Cursor, &input).unwrap();
        assert_eq!((call.session, call.gate), ("c1", Some(Gate::Subagent)));
    }
}
