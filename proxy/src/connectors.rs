//! Connectors for coding agents beyond Claude Code:
//!
//! - `GET /connect/{codex,gemini,cursor}.py`: stdlib installers that point the agent's own
//!   telemetry and hooks at Tokenrein (see docs/connectors.md).
//! - The optional Cursor usage import: a team's usage events, read from Cursor's Admin API
//!   (`POST /teams/filtered-usage-events`) with an Admin API key saved encrypted like provider
//!   keys, charged to one project's budgets. Runs hourly and on demand.

use std::sync::Arc;
use std::time::Duration;

use axum::extract::{Path, State};
use axum::http::{HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::{TimeZone, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::admin::{edit_error, err, find_org, unavailable};
use crate::audit;
use crate::auth::{Admin, Role};
use crate::config::{CursorImport, ProviderSecret, sha256_hex, valid_provider_key, valid_user};
use crate::control;
use crate::ingest::{Caller, Tool, UsageEvent, Who, record_usage, session_runs, to_id};
use crate::ledger::{Keys, unix_now};
use crate::proxy::AppState;

const CONNECT_COMMON: &str = include_str!("connect_common.py");
const CONNECT_HOOK: &str = include_str!("connect_hook.py");
const CONNECT_CODEX: &str = include_str!("connect_codex.py");
const CONNECT_GEMINI: &str = include_str!("connect_gemini.py");
const CONNECT_CURSOR: &str = include_str!("connect_cursor.py");

/// A connect script with the shared code and the hook built in. Holds no secrets; the key is
/// passed as an argument.
pub fn connect_script(tool: &str) -> Option<String> {
    let body = match tool {
        "codex" => CONNECT_CODEX,
        "gemini" => CONNECT_GEMINI,
        "cursor" => CONNECT_CURSOR,
        _ => return None,
    };
    let common = CONNECT_COMMON.replace("@tokenrein-hook@", CONNECT_HOOK);
    Some(body.replacen("# @tokenrein-common@\n", &common, 1))
}

fn script_response(tool: &str) -> Response {
    match connect_script(tool) {
        Some(s) => (
            [(
                axum::http::header::CONTENT_TYPE,
                HeaderValue::from_static("text/x-python; charset=utf-8"),
            )],
            s,
        )
            .into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

/// Routes for the admin listener.
pub fn routes() -> Router<Arc<AppState>> {
    Router::new()
        .route(
            "/connect/codex.py",
            get(|| async { script_response("codex") }),
        )
        .route(
            "/connect/gemini.py",
            get(|| async { script_response("gemini") }),
        )
        .route(
            "/connect/cursor.py",
            get(|| async { script_response("cursor") }),
        )
        .route(
            "/admin/orgs/{org}/connectors/cursor",
            get(cursor_status).put(set_cursor).delete(remove_cursor),
        )
        .route("/admin/orgs/{org}/connectors/cursor/sync", post(sync_now))
}

// --- Cursor usage import ---

const SOURCE: &str = "cursor";
/// Each sync re-reads this much before the last one, for events Cursor reports late (it
/// aggregates hourly); events already imported are skipped.
const OVERLAP_MS: i64 = 3 * 3600 * 1000;
/// Never reach back further than this.
const MAX_WINDOW_MS: i64 = 7 * 24 * 3600 * 1000;
const SEEN_TTL_SECS: u64 = 10 * 24 * 3600;
const PAGE_SIZE: u32 = 500;
const MAX_PAGES: u32 = 40;

/// Progress of an org's import, kept in Redis.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct ImportState {
    /// Events up to here (epoch ms) have been read.
    pub through_ms: Option<i64>,
    pub last_run: Option<String>,
    /// Events charged by the last run.
    pub imported: u64,
    /// Why the last run failed.
    pub error: Option<String>,
}

#[derive(Debug)]
pub enum SyncError {
    NotConfigured,
    /// Cannot run until an admin fixes something (missing project, secrets key, rejected key).
    Setup(String),
    /// Cursor's API failed or answered something unexpected.
    Upstream(String),
    Ledger(anyhow::Error),
}

impl SyncError {
    fn message(&self) -> String {
        match self {
            SyncError::NotConfigured => "the Cursor import is not set up".into(),
            SyncError::Setup(m) | SyncError::Upstream(m) => m.clone(),
            SyncError::Ledger(_) => "budget ledger unavailable".into(),
        }
    }
}

async fn load_state(st: &AppState, org: &str) -> anyhow::Result<ImportState> {
    Ok(st
        .ledger
        .get_value(&Keys::new(org).import_state(SOURCE))
        .await?
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default())
}

async fn save_state(st: &AppState, org: &str, s: &ImportState) -> anyhow::Result<()> {
    st.ledger
        .set_value(
            &Keys::new(org).import_state(SOURCE),
            &serde_json::to_string(s)?,
        )
        .await
}

/// Read new usage events from Cursor and charge them to the org's chosen project.
pub async fn sync_cursor(st: &AppState, org: &str) -> Result<ImportState, SyncError> {
    let mut state = load_state(st, org).await.map_err(SyncError::Ledger)?;
    let result = run_import(st, org, &state).await;
    state.last_run = Some(Utc::now().to_rfc3339());
    match &result {
        Ok((through, imported)) => {
            state.through_ms = Some(*through);
            state.imported = *imported;
            state.error = None;
        }
        Err(SyncError::NotConfigured | SyncError::Ledger(_)) => {}
        Err(e) => {
            state.imported = 0;
            state.error = Some(e.message());
        }
    }
    if !matches!(result, Err(SyncError::NotConfigured)) {
        save_state(st, org, &state)
            .await
            .map_err(SyncError::Ledger)?;
    }
    result.map(|_| state)
}

async fn run_import(st: &AppState, org: &str, prev: &ImportState) -> Result<(i64, u64), SyncError> {
    let rt = st.rt();
    let oi = rt
        .cfg
        .orgs
        .iter()
        .position(|o| o.id == org)
        .ok_or(SyncError::NotConfigured)?;
    let imp = rt.cfg.orgs[oi]
        .cursor_import
        .clone()
        .ok_or(SyncError::NotConfigured)?;
    let pi = rt.cfg.orgs[oi]
        .projects
        .iter()
        .position(|p| p.id == imp.project)
        .ok_or_else(|| {
            SyncError::Setup(format!(
                "project {:?} no longer exists; choose another one for the Cursor import",
                imp.project
            ))
        })?;
    let secrets = rt.secrets().ok_or_else(|| {
        SyncError::Setup(format!(
            "{} is not set on the proxy, so the saved Cursor key cannot be decrypted",
            rt.cfg.server.secrets_key_env
        ))
    })?;
    let key = secrets
        .open(
            &crate::secrets::connector_context(org, SOURCE),
            &imp.key.sealed,
        )
        .map_err(|e| SyncError::Setup(format!("saved Cursor key cannot be used: {e}")))?;
    let base = rt
        .cfg
        .server
        .cursor_api_url
        .trim_end_matches('/')
        .to_string();
    let now_ms = Utc::now().timestamp_millis();
    let start_of_day = Utc::now()
        .date_naive()
        .and_hms_opt(0, 0, 0)
        .map(|d| d.and_utc().timestamp_millis())
        .unwrap_or(now_ms);
    // The first run starts at the beginning of today (UTC), so today's budgets see today's use.
    let start = prev
        .through_ms
        .map(|t| t - OVERLAP_MS)
        .unwrap_or(start_of_day)
        .max(now_ms - MAX_WINDOW_MS);
    let c = Caller {
        rt: rt.clone(),
        oi,
        pi,
    };
    let mut imported = 0u64;
    for page in 1..=MAX_PAGES {
        let resp = st
            .http
            .post(format!("{base}/teams/filtered-usage-events"))
            .basic_auth(&key, Some(""))
            .timeout(Duration::from_secs(30))
            .json(&json!({
                "startDate": start,
                "endDate": now_ms,
                "page": page,
                "pageSize": PAGE_SIZE,
            }))
            .send()
            .await
            .map_err(|e| {
                SyncError::Upstream(if e.is_timeout() {
                    "Cursor's Admin API did not answer in 30 seconds".into()
                } else {
                    "could not reach Cursor's Admin API".into()
                })
            })?;
        let status = resp.status();
        if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
            return Err(SyncError::Setup(format!(
                "Cursor rejected the Admin API key (HTTP {})",
                status.as_u16()
            )));
        }
        if !status.is_success() {
            return Err(SyncError::Upstream(format!(
                "Cursor's Admin API answered HTTP {}",
                status.as_u16()
            )));
        }
        let body: Value = resp.json().await.map_err(|_| {
            SyncError::Upstream("Cursor's Admin API answered something other than JSON".into())
        })?;
        let events = body
            .get("usageEvents")
            .and_then(Value::as_array)
            .ok_or_else(|| SyncError::Upstream("Cursor's answer has no usageEvents list".into()))?;
        for ev in events {
            if import_event(st, &c, org, ev)
                .await
                .map_err(SyncError::Ledger)?
            {
                imported += 1;
            }
        }
        let more = body
            .pointer("/pagination/hasNextPage")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if !more || events.is_empty() {
            break;
        }
    }
    Ok((now_ms, imported))
}

/// Cursor's usage event as a reported model call. `None` for events without a model.
fn read_event(ev: &Value) -> Option<(UsageEvent, Who)> {
    let s = |k: &str| ev.get(k).and_then(Value::as_str).filter(|v| !v.is_empty());
    let n = |p: &str| ev.pointer(p).and_then(Value::as_f64);
    let tokens = |p: &str| n(p).unwrap_or(0.0).max(0.0) as u64;
    let model = s("model")?;
    let ts_ms = s("timestamp")
        .and_then(|t| t.parse::<i64>().ok())
        .or_else(|| ev.get("timestamp").and_then(Value::as_i64))?;
    let when = Utc.timestamp_millis_opt(ts_ms).single()?;
    // chargedCents matches Cursor's spend totals (model cost plus any Cursor token fee).
    let cents = n("/chargedCents").or_else(|| n("/tokenUsage/totalCents"));
    let user = s("userEmail")
        .or_else(|| s("serviceAccountName"))
        .filter(|u| valid_user(u))
        .map(str::to_string);
    let (run, parent) = match s("conversationId").or_else(|| s("cloudAgentId")) {
        Some(conv) => session_runs(Tool::Cursor, conv, false)?,
        // Usage outside a conversation (e.g. Tab): one run per user and day.
        None => (
            to_id(&format!(
                "cursor-{}-{}",
                user.as_deref().unwrap_or("team"),
                when.format("%Y%m%d")
            ))?,
            None,
        ),
    };
    let usage = UsageEvent {
        model: model.chars().take(200).collect(),
        input_tokens: tokens("/tokenUsage/inputTokens"),
        output_tokens: tokens("/tokenUsage/outputTokens"),
        cache_read_tokens: tokens("/tokenUsage/cacheReadTokens"),
        cache_write_tokens: tokens("/tokenUsage/cacheWriteTokens"),
        cost_usd: cents
            .filter(|c| c.is_finite() && *c >= 0.0)
            .map(|c| c / 100.0),
        ts: Some(when.to_rfc3339()),
        ..UsageEvent::default()
    };
    let who = Who {
        run,
        parent,
        agent: Tool::Cursor.name().into(),
        user,
        source: SOURCE.into(),
        run_budget_usd: None,
    };
    Some((usage, who))
}

/// Charge one event unless it was imported before. True if it was charged now.
async fn import_event(st: &AppState, c: &Caller, org: &str, ev: &Value) -> anyhow::Result<bool> {
    let Some((mut usage, who)) = read_event(ev) else {
        return Ok(false);
    };
    let id = sha256_hex(&ev.to_string());
    if !st
        .ledger
        .first_time(&Keys::new(org).import_seen(SOURCE, &id), SEEN_TTL_SECS)
        .await?
    {
        return Ok(false);
    }
    usage.request_id = Some(format!("cursor_{}", &id[..24]));
    record_usage(st, c, &usage, &who).await?;
    Ok(true)
}

/// Start an org's hourly import; one instance runs it per org and hour.
pub async fn sync_due(st: &Arc<AppState>, org: &str) {
    let lock = format!(
        "{}:hour:{}",
        Keys::new(org).import_state(SOURCE),
        unix_now() / 3600
    );
    match st.ledger.first_time(&lock, 7200).await {
        Ok(true) => {}
        Ok(false) => return,
        Err(e) => {
            tracing::warn!(org, error = %e, "cursor import lock failed");
            return;
        }
    }
    let (st, org) = (st.clone(), org.to_string());
    tokio::spawn(async move {
        match sync_cursor(&st, &org).await {
            Ok(s) => tracing::info!(org = %org, imported = s.imported, "cursor usage imported"),
            Err(e) => tracing::warn!(org = %org, error = %e.message(), "cursor import failed"),
        }
    });
}

// --- Admin API ---

async fn cursor_status(
    State(st): State<Arc<AppState>>,
    Path(org): Path<String>,
    admin: Admin,
) -> Response {
    if let Err(r) = admin.org(&st, &org, Role::Viewer) {
        return r;
    }
    let rt = st.rt();
    let imp = rt
        .cfg
        .orgs
        .iter()
        .find(|o| o.id == org)
        .and_then(|o| o.cursor_import.as_ref());
    let state = match load_state(&st, &org).await {
        Ok(s) => s,
        Err(e) => return unavailable(e),
    };
    Json(json!({
        "configured": imp.is_some(),
        "project": imp.map(|i| &i.project),
        "last4": imp.map(|i| &i.key.last4),
        "updated_at": imp.map(|i| &i.key.updated_at),
        "secrets_enabled": rt.secrets().is_some(),
        "secrets_key_env": rt.cfg.server.secrets_key_env,
        "sync": if imp.is_some() { json!(state) } else { Value::Null },
    }))
    .into_response()
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CursorBody {
    project: String,
    /// Omit to keep the saved key and only change the project.
    api_key: Option<String>,
}

async fn set_cursor(
    State(st): State<Arc<AppState>>,
    Path(org): Path<String>,
    admin: Admin,
    Json(body): Json<CursorBody>,
) -> Response {
    if let Err(r) = admin.org(&st, &org, Role::Admin) {
        return r;
    }
    let rt = st.rt();
    let sealed = match body.api_key.as_deref().map(str::trim) {
        None => None,
        Some(key) => {
            let Some(secrets) = rt.secrets() else {
                return err(
                    StatusCode::CONFLICT,
                    &format!(
                        "set {} on the proxy (e.g. `openssl rand -hex 32`) and restart it to save the Cursor key",
                        rt.cfg.server.secrets_key_env
                    ),
                );
            };
            if !valid_provider_key(key) {
                return err(
                    StatusCode::BAD_REQUEST,
                    "API key must be 8-512 printable characters without spaces",
                );
            }
            match secrets.seal(&crate::secrets::connector_context(&org, SOURCE), key) {
                Ok(s) => Some(ProviderSecret {
                    sealed: s,
                    last4: key.chars().skip(key.chars().count() - 4).collect(),
                    updated_at: Utc::now().to_rfc3339(),
                }),
                Err(e) => return unavailable(e),
            }
        }
    };
    let project = body.project.clone();
    let result = control::edit(
        &st,
        admin.change("cursor_import.set", Some(&org), SOURCE),
        |orgs| {
            let o = find_org(orgs, &org)?;
            if !o.projects.iter().any(|p| p.id == project) {
                return Err(control::EditError::NotFound(format!(
                    "unknown project {org}/{project}"
                )));
            }
            let key = match (sealed, o.cursor_import.take()) {
                (Some(k), _) => k,
                (None, Some(old)) => old.key,
                (None, None) => {
                    return Err(control::EditError::Invalid(
                        "api_key is required the first time".into(),
                    ));
                }
            };
            o.cursor_import = Some(CursorImport { project, key });
            Ok(())
        },
    )
    .await;
    match result {
        Ok(()) => cursor_status(State(st), Path(org), admin).await,
        Err(e) => edit_error(e),
    }
}

async fn remove_cursor(
    State(st): State<Arc<AppState>>,
    Path(org): Path<String>,
    admin: Admin,
) -> Response {
    if let Err(r) = admin.org(&st, &org, Role::Admin) {
        return r;
    }
    let result = control::edit(
        &st,
        admin.change("cursor_import.remove", Some(&org), SOURCE),
        |orgs| match find_org(orgs, &org)?.cursor_import.take() {
            Some(_) => Ok(()),
            None => Err(control::EditError::NotFound(
                "the Cursor import is not set up".into(),
            )),
        },
    )
    .await;
    match result {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => edit_error(e),
    }
}

async fn sync_now(
    State(st): State<Arc<AppState>>,
    Path(org): Path<String>,
    admin: Admin,
) -> Response {
    if let Err(r) = admin.org(&st, &org, Role::Admin) {
        return r;
    }
    let result = sync_cursor(&st, &org).await;
    let detail = match &result {
        Ok(s) => json!({ "ok": true, "imported": s.imported }),
        Err(e) => json!({ "ok": false, "error": e.message() }),
    };
    audit::record(&st, admin.entry("cursor_import.sync", &org, SOURCE, detail)).await;
    match result {
        Ok(s) => Json(json!(s)).into_response(),
        Err(SyncError::NotConfigured) => {
            err(StatusCode::NOT_FOUND, "the Cursor import is not set up")
        }
        Err(SyncError::Setup(m)) => err(StatusCode::CONFLICT, &m),
        Err(SyncError::Upstream(m)) => err(StatusCode::BAD_GATEWAY, &m),
        Err(SyncError::Ledger(e)) => unavailable(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scripts_are_assembled() {
        for tool in ["codex", "gemini", "cursor"] {
            let s = connect_script(tool).unwrap();
            assert!(s.starts_with("#!/usr/bin/env python3"));
            assert!(!s.contains("@tokenrein-"), "{tool}: placeholder left");
            assert!(s.contains("def install_hook"));
            assert!(s.contains("/v1/hooks/"));
        }
        // The hook is embedded in a raw ''' string.
        assert!(!CONNECT_HOOK.contains("'''"));
        assert!(connect_script("other").is_none());
    }

    #[test]
    fn reads_cursor_usage_events() {
        let (u, who) = read_event(&json!({
            "timestamp": "1750979225854",
            "userEmail": "developer@company.com",
            "conversationId": "8f2e4a1b-6c3d-4e5f-9a7b-2d1c8e6f4a3b",
            "model": "claude-4.5-sonnet",
            "kind": "Usage-based",
            "isTokenBasedCall": true,
            "tokenUsage": {"inputTokens": 126, "outputTokens": 450, "cacheWriteTokens": 6112,
                           "cacheReadTokens": 11964, "totalCents": 20.18232},
            "chargedCents": 21.36232
        }))
        .unwrap();
        assert_eq!(who.run, "cursor-8f2e4a1b-6c3d-4e5f-9a7b-2d1c8e6f4a3b");
        assert_eq!(who.user.as_deref(), Some("developer@company.com"));
        assert_eq!((u.input_tokens, u.cache_write_tokens), (126, 6112));
        assert!((u.cost_usd.unwrap() - 0.2136232).abs() < 1e-9);
        assert!(u.ts.unwrap().starts_with("2025-06-26"));
        // No conversation: one run per user and day.
        let (_, who) = read_event(&json!({
            "timestamp": "1750978339901", "userEmail": "admin@company.com",
            "model": "claude-4-sonnet-thinking", "chargedCents": 8
        }))
        .unwrap();
        assert_eq!(who.run, "cursor-admin-company.com-20250626");
        assert!(read_event(&json!({"timestamp": "1"})).is_none());
    }
}
