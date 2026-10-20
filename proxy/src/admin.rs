use std::collections::HashMap;
use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::header::{AUTHORIZATION, CACHE_CONTROL, CONTENT_SECURITY_POLICY};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{delete, get, post, put};
use axum::{Json, Router};
use chrono::Utc;
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::config::{
    Budgets, FailMode, OrgConfig, ProjectConfig, ProviderSecret, VirtualKey, generate_virtual_key,
    valid_id, valid_provider_key,
};
use crate::control::{self, EditError};
use crate::ledger::Keys;
use crate::providers::Provider;
use crate::proxy::AppState;

const DASHBOARD_HTML: &str = include_str!("dashboard.html");

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/", get(dashboard))
        .route("/healthz", get(|| async { "ok" }))
        .route("/readyz", get(readyz))
        .route("/admin/overview", get(overview))
        .route("/admin/packs", get(list_packs))
        .route(
            "/admin/orgs/{org}/projects/{project}/pack",
            post(apply_pack),
        )
        .route(
            "/admin/orgs/{org}/provider-keys/{provider}/test",
            post(test_provider_key),
        )
        .route("/admin/orgs/{org}/review", get(review))
        .route("/admin/orgs/{org}/spend", get(spend))
        .route("/admin/orgs/{org}/runs/{run}/timeline", get(run_timeline))
        .route("/admin/orgs", post(create_org))
        .route("/admin/orgs/{org}", put(update_org).delete(delete_org))
        .route(
            "/admin/orgs/{org}/provider-keys/{provider}",
            put(set_provider_key).delete(remove_provider_key),
        )
        .route("/admin/orgs/{org}/projects", post(create_project))
        .route(
            "/admin/orgs/{org}/projects/{project}",
            put(update_project).delete(delete_project),
        )
        .route(
            "/admin/orgs/{org}/projects/{project}/keys",
            post(create_key),
        )
        .route(
            "/admin/orgs/{org}/projects/{project}/keys/{key_id}",
            delete(revoke_key),
        )
        .route("/admin/orgs/{org}/runs", get(list_runs))
        .route("/admin/orgs/{org}/agents", get(list_agents))
        .route("/admin/orgs/{org}/events", get(list_events))
        .route("/admin/orgs/{org}/runs/{run}", get(get_run))
        .route("/admin/orgs/{org}/runs/{run}/pause", post(pause))
        .route("/admin/orgs/{org}/runs/{run}/resume", post(resume))
        .route("/admin/orgs/{org}/runs/{run}/kill", post(kill))
        .with_state(state)
}

/// The dashboard page. It holds no data; it calls the admin API with the token the user enters.
async fn dashboard() -> Response {
    let mut resp = Html(DASHBOARD_HTML).into_response();
    let h = resp.headers_mut();
    h.insert(
        CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(
            "default-src 'none'; script-src 'unsafe-inline'; style-src 'unsafe-inline'; \
             connect-src 'self'; img-src 'self' data:; frame-ancestors 'none'; base-uri 'none'; form-action 'none'",
        ),
    );
    h.insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    resp
}

async fn readyz(State(st): State<Arc<AppState>>) -> Response {
    match st.ledger.ping().await {
        Ok(()) => (StatusCode::OK, "ready").into_response(),
        Err(_) => (StatusCode::SERVICE_UNAVAILABLE, "redis unavailable").into_response(),
    }
}

fn err(status: StatusCode, message: &str) -> Response {
    (status, Json(json!({ "error": message }))).into_response()
}

#[allow(clippy::result_large_err)]
fn check_token(st: &AppState, headers: &HeaderMap) -> Result<(), Response> {
    let rt = st.rt();
    let Some(expected) = rt.admin_token.as_deref() else {
        return Err(err(
            StatusCode::SERVICE_UNAVAILABLE,
            "admin API is disabled: set the admin token environment variable",
        ));
    };
    let given = headers
        .get(AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or("");
    // Compare digests so the check takes the same time regardless of where the strings differ.
    let (a, b) = (
        Sha256::digest(given.as_bytes()),
        Sha256::digest(expected.as_bytes()),
    );
    if a.iter()
        .zip(b.iter())
        .fold(0u8, |acc, (x, y)| acc | (x ^ y))
        != 0
    {
        return Err(err(StatusCode::UNAUTHORIZED, "invalid admin token"));
    }
    Ok(())
}

#[allow(clippy::result_large_err)]
fn authorize_org(st: &AppState, headers: &HeaderMap, org: &str) -> Result<(), Response> {
    check_token(st, headers)?;
    if !st.rt().cfg.orgs.iter().any(|o| o.id == org) {
        return Err(err(StatusCode::NOT_FOUND, "unknown org"));
    }
    Ok(())
}

#[allow(clippy::result_large_err)]
fn authorize(st: &AppState, headers: &HeaderMap, org: &str, run: &str) -> Result<(), Response> {
    authorize_org(st, headers, org)?;
    if !valid_id(run) {
        return Err(err(StatusCode::BAD_REQUEST, "invalid run id"));
    }
    Ok(())
}

fn unavailable(e: anyhow::Error) -> Response {
    tracing::error!(error = %e, "admin ledger call failed");
    err(StatusCode::SERVICE_UNAVAILABLE, "budget ledger unavailable")
}

fn edit_error(e: EditError) -> Response {
    match e {
        EditError::Invalid(m) => err(StatusCode::BAD_REQUEST, &m),
        EditError::NotFound(m) => err(StatusCode::NOT_FOUND, &m),
        EditError::Disabled => err(
            StatusCode::CONFLICT,
            "managed config is not initialised on this proxy",
        ),
        EditError::Conflict => err(
            StatusCode::CONFLICT,
            "the config was changed at the same time by someone else; reload and try again",
        ),
        EditError::Unavailable(e) => unavailable(e),
    }
}

fn state_json(org: &str, run: &str, state: &str) -> Response {
    Json(json!({ "org": org, "run_id": run, "state": state })).into_response()
}

/// A budget hash from the ledger as JSON (micro-USD).
fn budget_json(b: &HashMap<String, String>) -> Value {
    let micros = |field: &str| {
        b.get(field)
            .and_then(|v| v.parse::<f64>().ok())
            .map(|f| f as i64)
    };
    json!({
        "mode": b.get("mode"),
        "limit_micros": micros("limit").filter(|l| *l >= 0),
        "spent_micros": micros("spent").unwrap_or(0),
        "reserved_micros": micros("reserved").unwrap_or(0),
    })
}

fn key_id(hash: &str) -> String {
    hash.chars().take(12).collect()
}

// --- Overview: orgs, projects, keys, budgets and this month's spend ---

async fn overview(State(st): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if let Err(r) = check_token(&st, &headers) {
        return r;
    }
    let rt = st.rt();
    let now = Utc::now();
    let ym = now.format("%Y%m").to_string();
    let mut budget_keys = Vec::new();
    for org in &rt.cfg.orgs {
        let keys = Keys::new(&org.id);
        budget_keys.push(keys.org_monthly(&ym));
        for p in &org.projects {
            budget_keys.push(keys.project_monthly(&p.id, &ym));
        }
    }
    let spend = match st.ledger.budgets(&budget_keys).await {
        Ok(s) => s,
        Err(e) => return unavailable(e),
    };
    let mut spend = spend.iter();
    let orgs: Vec<Value> = rt
        .cfg
        .orgs
        .iter()
        .enumerate()
        .map(|(oi, org)| {
            let providers: serde_json::Map<String, Value> = [Provider::OpenAI, Provider::Anthropic]
                .into_iter()
                .map(|p| {
                    let status = rt.provider_status(oi, p).map_or(Value::Null, |s| json!(s));
                    (p.name().to_string(), status)
                })
                .collect();
            let org_spend = spend.next().map(budget_json);
            let projects: Vec<Value> = org
                .projects
                .iter()
                .map(|p| {
                    let mut keys: Vec<Value> = p
                        .keys
                        .iter()
                        .map(|k| {
                            json!({
                                "id": key_id(&k.hash),
                                "name": k.name,
                                "prefix": k.prefix,
                                "created_at": k.created_at,
                                "source": "dashboard",
                            })
                        })
                        .collect();
                    keys.extend(p.virtual_key_hashes.iter().map(|h| {
                        json!({ "id": key_id(h), "name": "from config file", "prefix": null, "created_at": null, "source": "config" })
                    }));
                    json!({
                        "id": p.id,
                        "pack": p.pack,
                        "budgets": p.budgets,
                        "effective": Budgets::effective(&org.budgets, &p.budgets),
                        "keys": keys,
                        "spend": { "project_monthly": spend.next().map(budget_json) },
                    })
                })
                .collect();
            json!({
                "id": org.id,
                "providers": providers,
                "fail_mode": org.fail_mode,
                "budgets": org.budgets,
                "spend": { "org_monthly": org_spend },
                "projects": projects,
            })
        })
        .collect();
    let version = st.control_version.load(std::sync::atomic::Ordering::SeqCst);
    Json(json!({
        "version": version,
        "managed": version > 0,
        "secrets_enabled": rt.secrets().is_some(),
        "secrets_key_env": rt.cfg.server.secrets_key_env,
        "month": ym,
        "day": now.format("%Y%m%d").to_string(),
        "orgs": orgs,
    }))
    .into_response()
}

// --- Editing orgs, projects, keys and budgets ---

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct OrgBody {
    id: Option<String>,
    fail_mode: Option<FailMode>,
    budgets: Option<Budgets>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ProjectBody {
    id: Option<String>,
    budgets: Option<Budgets>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct KeyBody {
    name: String,
}

fn find_org<'a>(orgs: &'a mut [OrgConfig], org: &str) -> Result<&'a mut OrgConfig, EditError> {
    orgs.iter_mut()
        .find(|o| o.id == org)
        .ok_or_else(|| EditError::NotFound(format!("unknown org {org:?}")))
}

fn find_project<'a>(
    orgs: &'a mut [OrgConfig],
    org: &str,
    project: &str,
) -> Result<&'a mut ProjectConfig, EditError> {
    find_org(orgs, org)?
        .projects
        .iter_mut()
        .find(|p| p.id == project)
        .ok_or_else(|| EditError::NotFound(format!("unknown project {org}/{project}")))
}

fn new_id(id: Option<String>, kind: &str) -> Result<String, EditError> {
    match id {
        Some(id) if valid_id(&id) => Ok(id),
        _ => Err(EditError::Invalid(format!(
            "{kind} id must be 1-128 letters, digits, '.', '_' or '-'"
        ))),
    }
}

async fn create_org(
    State(st): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<OrgBody>,
) -> Response {
    if let Err(r) = check_token(&st, &headers) {
        return r;
    }
    let result = control::edit(&st, |orgs| {
        let id = new_id(body.id, "org")?;
        if orgs.iter().any(|o| o.id == id) {
            return Err(EditError::Invalid(format!("org {id:?} already exists")));
        }
        orgs.push(OrgConfig {
            id: id.clone(),
            fail_mode: body.fail_mode.unwrap_or_default(),
            budgets: body.budgets.unwrap_or_default(),
            provider_key_env: HashMap::new(),
            provider_secrets: HashMap::new(),
            projects: Vec::new(),
        });
        Ok(id)
    })
    .await;
    match result {
        Ok(id) => (StatusCode::CREATED, Json(json!({ "id": id }))).into_response(),
        Err(e) => edit_error(e),
    }
}

async fn update_org(
    State(st): State<Arc<AppState>>,
    Path(org): Path<String>,
    headers: HeaderMap,
    Json(body): Json<OrgBody>,
) -> Response {
    if let Err(r) = authorize_org(&st, &headers, &org) {
        return r;
    }
    let result = control::edit(&st, |orgs| {
        if body.id.as_deref().is_some_and(|id| id != org) {
            return Err(EditError::Invalid("org ids cannot be changed".into()));
        }
        let o = find_org(orgs, &org)?;
        if let Some(fm) = body.fail_mode {
            o.fail_mode = fm;
        }
        if let Some(b) = body.budgets {
            o.budgets = b;
        }
        Ok(())
    })
    .await;
    match result {
        Ok(()) => Json(json!({ "id": org })).into_response(),
        Err(e) => edit_error(e),
    }
}

async fn delete_org(
    State(st): State<Arc<AppState>>,
    Path(org): Path<String>,
    headers: HeaderMap,
) -> Response {
    if let Err(r) = authorize_org(&st, &headers, &org) {
        return r;
    }
    let result = control::edit(&st, |orgs| {
        orgs.retain(|o| o.id != org);
        Ok(())
    })
    .await;
    match result {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => edit_error(e),
    }
}

async fn create_project(
    State(st): State<Arc<AppState>>,
    Path(org): Path<String>,
    headers: HeaderMap,
    Json(body): Json<ProjectBody>,
) -> Response {
    if let Err(r) = authorize_org(&st, &headers, &org) {
        return r;
    }
    let result = control::edit(&st, |orgs| {
        let id = new_id(body.id, "project")?;
        let o = find_org(orgs, &org)?;
        if o.projects.iter().any(|p| p.id == id) {
            return Err(EditError::Invalid(format!("project {id:?} already exists")));
        }
        o.projects.push(ProjectConfig {
            id: id.clone(),
            virtual_key_hashes: Vec::new(),
            keys: Vec::new(),
            budgets: body.budgets.unwrap_or_default(),
            pack: None,
        });
        Ok(id)
    })
    .await;
    match result {
        Ok(id) => (StatusCode::CREATED, Json(json!({ "id": id }))).into_response(),
        Err(e) => edit_error(e),
    }
}

async fn update_project(
    State(st): State<Arc<AppState>>,
    Path((org, project)): Path<(String, String)>,
    headers: HeaderMap,
    Json(body): Json<ProjectBody>,
) -> Response {
    if let Err(r) = authorize_org(&st, &headers, &org) {
        return r;
    }
    let result = control::edit(&st, |orgs| {
        if body.id.as_deref().is_some_and(|id| id != project) {
            return Err(EditError::Invalid("project ids cannot be changed".into()));
        }
        let p = find_project(orgs, &org, &project)?;
        if let Some(mut b) = body.budgets {
            // Only the org can set an org-wide budget.
            b.org_monthly = None;
            p.budgets = b;
        }
        Ok(())
    })
    .await;
    match result {
        Ok(()) => Json(json!({ "id": project })).into_response(),
        Err(e) => edit_error(e),
    }
}

async fn delete_project(
    State(st): State<Arc<AppState>>,
    Path((org, project)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    if let Err(r) = authorize_org(&st, &headers, &org) {
        return r;
    }
    let result = control::edit(&st, |orgs| {
        let o = find_org(orgs, &org)?;
        let before = o.projects.len();
        o.projects.retain(|p| p.id != project);
        if o.projects.len() == before {
            return Err(EditError::NotFound(format!(
                "unknown project {org}/{project}"
            )));
        }
        Ok(())
    })
    .await;
    match result {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => edit_error(e),
    }
}

async fn create_key(
    State(st): State<Arc<AppState>>,
    Path((org, project)): Path<(String, String)>,
    headers: HeaderMap,
    Json(body): Json<KeyBody>,
) -> Response {
    if let Err(r) = authorize_org(&st, &headers, &org) {
        return r;
    }
    let name = body.name.trim().to_string();
    if name.is_empty() || name.len() > 100 {
        return err(StatusCode::BAD_REQUEST, "key name must be 1-100 characters");
    }
    let (key, hash) = generate_virtual_key();
    let vk = VirtualKey {
        hash: hash.clone(),
        name,
        prefix: key.chars().take(11).collect(),
        created_at: Utc::now().to_rfc3339(),
    };
    let shown = json!({
        "id": key_id(&hash),
        "name": vk.name,
        "prefix": vk.prefix,
        "created_at": vk.created_at,
    });
    let result = control::edit(&st, |orgs| {
        find_project(orgs, &org, &project)?.keys.push(vk);
        Ok(())
    })
    .await;
    match result {
        // The key itself is returned once and never stored.
        Ok(()) => {
            let mut body = shown;
            body["key"] = json!(key);
            (StatusCode::CREATED, Json(body)).into_response()
        }
        Err(e) => edit_error(e),
    }
}

async fn revoke_key(
    State(st): State<Arc<AppState>>,
    Path((org, project, key)): Path<(String, String, String)>,
    headers: HeaderMap,
) -> Response {
    if let Err(r) = authorize_org(&st, &headers, &org) {
        return r;
    }
    let result = control::edit(&st, |orgs| {
        let p = find_project(orgs, &org, &project)?;
        let before = p.keys.len() + p.virtual_key_hashes.len();
        p.keys.retain(|k| key_id(&k.hash) != key);
        p.virtual_key_hashes.retain(|h| key_id(h) != key);
        if p.keys.len() + p.virtual_key_hashes.len() == before {
            return Err(EditError::NotFound("unknown key".into()));
        }
        Ok(())
    })
    .await;
    match result {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => edit_error(e),
    }
}

// --- Provider API keys (stored encrypted; never returned) ---

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ProviderKeyBody {
    api_key: String,
}

async fn set_provider_key(
    State(st): State<Arc<AppState>>,
    Path((org, provider)): Path<(String, String)>,
    headers: HeaderMap,
    Json(body): Json<ProviderKeyBody>,
) -> Response {
    if let Err(r) = authorize_org(&st, &headers, &org) {
        return r;
    }
    let Some(p) = Provider::from_name(&provider) else {
        return err(StatusCode::NOT_FOUND, "unknown provider");
    };
    let rt = st.rt();
    if rt.upstream_base(p).is_none() {
        return err(
            StatusCode::CONFLICT,
            &format!("add a [providers.{provider}] section to the proxy config first"),
        );
    }
    let Some(secrets) = rt.secrets() else {
        return err(
            StatusCode::CONFLICT,
            &format!(
                "set {} on the proxy (e.g. `openssl rand -hex 32`) and restart it to save provider keys here",
                rt.cfg.server.secrets_key_env
            ),
        );
    };
    let key = body.api_key.trim();
    if !valid_provider_key(key) {
        return err(
            StatusCode::BAD_REQUEST,
            "API key must be 8-512 printable characters without spaces",
        );
    }
    let sealed = match secrets.seal(&crate::secrets::context(&org, p.name()), key) {
        Ok(s) => s,
        Err(e) => return unavailable(e),
    };
    let saved = ProviderSecret {
        sealed,
        last4: key
            .chars()
            .rev()
            .take(4)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect(),
        updated_at: Utc::now().to_rfc3339(),
    };
    let last4 = saved.last4.clone();
    let result = control::edit(&st, |orgs| {
        find_org(orgs, &org)?
            .provider_secrets
            .insert(p.name().to_string(), saved);
        Ok(())
    })
    .await;
    match result {
        Ok(()) => Json(json!({ "provider": p.name(), "source": "dashboard", "last4": last4 }))
            .into_response(),
        Err(e) => edit_error(e),
    }
}

async fn remove_provider_key(
    State(st): State<Arc<AppState>>,
    Path((org, provider)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    if let Err(r) = authorize_org(&st, &headers, &org) {
        return r;
    }
    let Some(p) = Provider::from_name(&provider) else {
        return err(StatusCode::NOT_FOUND, "unknown provider");
    };
    let result = control::edit(&st, |orgs| {
        match find_org(orgs, &org)?.provider_secrets.remove(p.name()) {
            Some(_) => Ok(()),
            None => Err(EditError::NotFound("no key saved for this provider".into())),
        }
    })
    .await;
    match result {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => edit_error(e),
    }
}

// --- Guardrail packs ---

async fn list_packs(State(st): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if let Err(r) = check_token(&st, &headers) {
        return r;
    }
    Json(json!({ "packs": crate::packs::all() })).into_response()
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PackBody {
    pack: String,
}

/// Replace a project's budgets with a pack's (all in Check mode).
async fn apply_pack(
    State(st): State<Arc<AppState>>,
    Path((org, project)): Path<(String, String)>,
    headers: HeaderMap,
    Json(body): Json<PackBody>,
) -> Response {
    if let Err(r) = authorize_org(&st, &headers, &org) {
        return r;
    }
    let Some(pack) = crate::packs::find(&body.pack) else {
        return err(StatusCode::NOT_FOUND, "unknown pack");
    };
    let result = control::edit(&st, |orgs| {
        let p = find_project(orgs, &org, &project)?;
        p.budgets = pack.budgets.clone();
        p.pack = Some(pack.id.to_string());
        Ok(())
    })
    .await;
    match result {
        Ok(()) => Json(json!({ "project": project, "pack": pack.id, "budgets": pack.budgets }))
            .into_response(),
        Err(e) => edit_error(e),
    }
}

// --- Provider key check ---

/// Ask the provider to list models with the org's key: free, and proves the key works.
async fn test_provider_key(
    State(st): State<Arc<AppState>>,
    Path((org, provider)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    if let Err(r) = authorize_org(&st, &headers, &org) {
        return r;
    }
    let Some(p) = Provider::from_name(&provider) else {
        return err(StatusCode::NOT_FOUND, "unknown provider");
    };
    let rt = st.rt();
    let Some(oi) = rt.cfg.orgs.iter().position(|o| o.id == org) else {
        return err(StatusCode::NOT_FOUND, "unknown org");
    };
    let (Some(base), Some(key)) = (rt.upstream_base(p), rt.provider_key(oi, p)) else {
        return Json(json!({ "ok": false, "message": "no key set for this provider" }))
            .into_response();
    };
    let req = st
        .http
        .get(format!("{base}/v1/models?limit=1"))
        .timeout(std::time::Duration::from_secs(10));
    let req = match p {
        Provider::OpenAI => req.bearer_auth(key),
        Provider::Anthropic => req
            .header("x-api-key", key)
            .header("anthropic-version", "2023-06-01"),
    };
    let (ok, status, message) = match req.send().await {
        Ok(r) if r.status().is_success() => {
            (true, Some(r.status().as_u16()), "key works".to_string())
        }
        Ok(r) if matches!(r.status().as_u16(), 401 | 403) => (
            false,
            Some(r.status().as_u16()),
            "the provider rejected this key".to_string(),
        ),
        Ok(r) => (
            false,
            Some(r.status().as_u16()),
            format!("the provider answered HTTP {}", r.status().as_u16()),
        ),
        Err(e) if e.is_timeout() => (
            false,
            None,
            "the provider did not answer in 10 seconds".into(),
        ),
        Err(_) => (false, None, "could not reach the provider".into()),
    };
    Json(json!({ "ok": ok, "status": status, "message": message })).into_response()
}

// --- Check-mode review: what Enforce would have blocked ---

#[derive(Default)]
struct ReviewGroup {
    requests: i64,
    cost_micros: i64,
    runs: Vec<String>,
    agents: Vec<String>,
    first_ts: Option<String>,
    last_ts: Option<String>,
}

async fn review(
    State(st): State<Arc<AppState>>,
    Path(org): Path<String>,
    Query(q): Query<ListQuery>,
    headers: HeaderMap,
) -> Response {
    if let Err(r) = authorize_org(&st, &headers, &org) {
        return r;
    }
    let events = match st
        .ledger
        .recent_events(&org, q.limit.unwrap_or(5000).clamp(100, 5000))
        .await
    {
        Ok(e) => e,
        Err(e) => return unavailable(e),
    };
    // (policy, project) -> group
    let mut groups: std::collections::BTreeMap<(String, String), ReviewGroup> = Default::default();
    // Per event: the limits it exceeded, and its cost.
    let mut event_groups: Vec<(Vec<(String, String)>, i64)> = Vec::new();
    for e in &events {
        let Some(labels) = e.get("check_exceeded").and_then(|v| v.as_array()) else {
            continue;
        };
        let s = |k: &str| e.get(k).and_then(|v| v.as_str()).unwrap_or("").to_string();
        // One request can exceed several scopes of the same limit (its run and every ancestor
        // run); count it once per limit.
        let mut hit: Vec<(String, String)> = Vec::new();
        for label in labels.iter().filter_map(|l| l.as_str()) {
            let (policy, project) = match label.split_once(':') {
                Some(("run", _)) => ("run", s("project")),
                Some(("agent_daily", rest)) => (
                    "agent_daily",
                    rest.split_once('/').map_or(rest, |(p, _)| p).to_string(),
                ),
                Some(("project_monthly", p)) => ("project_monthly", p.to_string()),
                _ if label == "org_monthly" => ("org_monthly", String::new()),
                _ => continue,
            };
            let key = (policy.to_string(), project);
            if !hit.contains(&key) {
                hit.push(key);
            }
        }
        if !hit.is_empty() {
            event_groups.push((
                hit.clone(),
                e.get("actual_micros").and_then(|v| v.as_i64()).unwrap_or(0),
            ));
        }
        for key in hit {
            let g = groups.entry(key).or_default();
            g.requests += 1;
            g.cost_micros += e.get("actual_micros").and_then(|v| v.as_i64()).unwrap_or(0);
            let run = s("run_id");
            if !g.runs.contains(&run) {
                g.runs.push(run);
            }
            let agent = s("agent");
            if !g.agents.contains(&agent) {
                g.agents.push(agent);
            }
            let ts = s("ts");
            // Events arrive newest first.
            if g.last_ts.is_none() {
                g.last_ts = Some(ts.clone());
            }
            g.first_ts = Some(ts);
        }
    }
    let rt = st.rt();
    let org_cfg = rt.cfg.orgs.iter().find(|o| o.id == org);
    let current_policy = |policy: &str, project: &str| {
        let o = org_cfg?;
        let p = o.projects.iter().find(|p| p.id == project);
        let b = Budgets::effective(&o.budgets, p.map_or(&Budgets::default(), |p| &p.budgets));
        match policy {
            "run" => b.run,
            "agent_daily" => b.agent_daily,
            "project_monthly" => b.project_monthly,
            "org_monthly" => b.org_monthly,
            _ => None,
        }
    };
    let in_check = |(policy, project): &(String, String)| {
        current_policy(policy, project).is_some_and(|c| c.mode == crate::config::Mode::Check)
    };
    let (mut total_requests, mut total_cost) = (0i64, 0i64);
    for (hit, cost) in &event_groups {
        if hit.iter().any(in_check) {
            total_requests += 1;
            total_cost += cost;
        }
    }
    let items: Vec<Value> = groups
        .into_iter()
        .map(|((policy, project), g)| {
            let current = current_policy(&policy, &project);
            json!({
                "policy": policy,
                "project": if project.is_empty() { Value::Null } else { json!(project) },
                "requests": g.requests,
                "cost_micros": g.cost_micros,
                "run_count": g.runs.len(),
                "runs": g.runs.iter().take(5).collect::<Vec<_>>(),
                "agents": g.agents.iter().take(5).collect::<Vec<_>>(),
                "first_ts": g.first_ts,
                "last_ts": g.last_ts,
                "current": current,
            })
        })
        .collect();
    Json(json!({
        "events_scanned": events.len(),
        "oldest_ts": events.last().and_then(|e| e.get("ts")),
        // Distinct requests that at least one limit still in Check mode would have blocked.
        "check_requests": total_requests,
        "check_cost_micros": total_cost,
        "items": items,
    }))
    .into_response()
}

// --- Spend analytics (from recorded activity; budgets stay authoritative in the ledger) ---

#[derive(Deserialize)]
struct SpendQuery {
    days: Option<u32>,
}

async fn spend(
    State(st): State<Arc<AppState>>,
    Path(org): Path<String>,
    Query(q): Query<SpendQuery>,
    headers: HeaderMap,
) -> Response {
    if let Err(r) = authorize_org(&st, &headers, &org) {
        return r;
    }
    let n = q.days.unwrap_or(30).clamp(1, 40) as i64;
    let today = Utc::now().date_naive();
    let days: Vec<String> = (0..n)
        .rev()
        .map(|i| {
            (today - chrono::Duration::days(i))
                .format("%Y%m%d")
                .to_string()
        })
        .collect();
    let hashes = match st.ledger.spend_days(&org, &days).await {
        Ok(h) => h,
        Err(e) => return unavailable(e),
    };
    // dim -> value -> [cost, requests, blocked]
    let mut by: HashMap<String, HashMap<String, [i64; 3]>> = HashMap::new();
    let mut series = Vec::with_capacity(days.len());
    for (day, h) in days.iter().zip(&hashes) {
        let mut total = [0i64; 3];
        for (field, v) in h {
            let Some((dim, rest)) = field.split_once(':') else {
                continue;
            };
            let Some((value, kind)) = rest.rsplit_once(':') else {
                continue;
            };
            let i = match kind {
                "c" => 0,
                "n" => 1,
                "b" => 2,
                _ => continue,
            };
            by.entry(dim.to_string())
                .or_default()
                .entry(value.to_string())
                .or_default()[i] += v;
            // Each request is counted once per dimension; total it from one of them.
            if dim == "project" {
                total[i] += v;
            }
        }
        series.push(json!({ "day": day, "cost_micros": total[0], "requests": total[1], "blocked": total[2] }));
    }
    let breakdown: serde_json::Map<String, Value> = ["model", "user", "project", "agent"]
        .into_iter()
        .map(|dim| {
            let mut rows: Vec<(String, [i64; 3])> = by.remove(dim).unwrap_or_default().into_iter().collect();
            rows.sort_by(|a, b| b.1[0].cmp(&a.1[0]).then(b.1[1].cmp(&a.1[1])));
            let rows: Vec<Value> = rows
                .into_iter()
                .take(50)
                .map(|(k, v)| json!({ "key": k, "cost_micros": v[0], "requests": v[1], "blocked": v[2] }))
                .collect();
            (dim.to_string(), json!(rows))
        })
        .collect();
    Json(json!({ "days": series, "by": breakdown })).into_response()
}

// --- Activity: runs, agents and request events ---

#[derive(Deserialize)]
struct ListQuery {
    limit: Option<usize>,
}

impl ListQuery {
    fn limit(&self, default: usize) -> usize {
        self.limit.unwrap_or(default).clamp(1, 1000)
    }
}

async fn list_runs(
    State(st): State<Arc<AppState>>,
    Path(org): Path<String>,
    Query(q): Query<ListQuery>,
    headers: HeaderMap,
) -> Response {
    if let Err(r) = authorize_org(&st, &headers, &org) {
        return r;
    }
    let recent = match st.ledger.recent_runs(&org, q.limit(200)).await {
        Ok(r) => r,
        Err(e) => return unavailable(e),
    };
    let ids: Vec<String> = recent.iter().map(|(r, _)| r.clone()).collect();
    let details = match st.ledger.run_details(&org, &ids).await {
        Ok(d) => d,
        Err(e) => return unavailable(e),
    };
    let runs: Vec<Value> = recent
        .iter()
        .zip(details)
        .map(|((run, last_seen), d)| {
            let num = |k: &str| d.info.get(k).and_then(|v| v.parse::<i64>().ok());
            json!({
                "run_id": run,
                "parent_run_id": d.parent.or_else(|| d.info.get("parent").cloned()),
                "state": d.state.unwrap_or_else(|| "running".into()),
                "project": d.info.get("project"),
                "agent": d.info.get("agent"),
                "model": d.info.get("model"),
                "last_decision": d.info.get("last_decision"),
                "requests": num("requests").unwrap_or(0),
                "blocked": num("blocked").unwrap_or(0),
                "first_seen": num("first_seen"),
                "last_seen": last_seen,
                "budget": budget_json(&d.budget),
            })
        })
        .collect();
    Json(json!({ "runs": runs })).into_response()
}

async fn list_agents(
    State(st): State<Arc<AppState>>,
    Path(org): Path<String>,
    Query(q): Query<ListQuery>,
    headers: HeaderMap,
) -> Response {
    if let Err(r) = authorize_org(&st, &headers, &org) {
        return r;
    }
    let recent = match st.ledger.recent_agents(&org, q.limit(200)).await {
        Ok(r) => r,
        Err(e) => return unavailable(e),
    };
    let ymd = Utc::now().format("%Y%m%d").to_string();
    let keys = Keys::new(&org);
    let pairs: Vec<(String, String, i64)> = recent
        .into_iter()
        .filter_map(|(member, ts)| {
            let (p, a) = member.split_once('/')?;
            Some((p.to_string(), a.to_string(), ts))
        })
        .collect();
    let budget_keys: Vec<String> = pairs
        .iter()
        .map(|(p, a, _)| keys.agent_daily(p, a, &ymd))
        .collect();
    let spend = match st.ledger.budgets(&budget_keys).await {
        Ok(s) => s,
        Err(e) => return unavailable(e),
    };
    let agents: Vec<Value> = pairs
        .iter()
        .zip(spend)
        .map(|((project, agent, ts), b)| {
            json!({ "project": project, "agent": agent, "last_seen": ts, "today": budget_json(&b) })
        })
        .collect();
    Json(json!({ "day": ymd, "agents": agents })).into_response()
}

async fn list_events(
    State(st): State<Arc<AppState>>,
    Path(org): Path<String>,
    Query(q): Query<ListQuery>,
    headers: HeaderMap,
) -> Response {
    if let Err(r) = authorize_org(&st, &headers, &org) {
        return r;
    }
    match st.ledger.recent_events(&org, q.limit(100)).await {
        Ok(events) => Json(json!({ "events": events })).into_response(),
        Err(e) => unavailable(e),
    }
}

async fn run_timeline(
    State(st): State<Arc<AppState>>,
    Path((org, run)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    if let Err(r) = authorize(&st, &headers, &org, &run) {
        return r;
    }
    let (runs, events) = match st.ledger.run_timeline(&org, &run, 200).await {
        Ok(t) => t,
        Err(e) => return unavailable(e),
    };
    let details = match st.ledger.run_details(&org, &runs).await {
        Ok(d) => d,
        Err(e) => return unavailable(e),
    };
    let runs: Vec<Value> = runs
        .iter()
        .zip(details)
        .map(|(id, d)| {
            let num = |k: &str| d.info.get(k).and_then(|v| v.parse::<i64>().ok());
            json!({
                "run_id": id,
                "parent_run_id": d.parent.or_else(|| d.info.get("parent").cloned()),
                "state": d.state.unwrap_or_else(|| "running".into()),
                "project": d.info.get("project"),
                "agent": d.info.get("agent"),
                "requests": num("requests").unwrap_or(0),
                "blocked": num("blocked").unwrap_or(0),
                "first_seen": num("first_seen"),
                "last_seen": num("last_seen"),
                "budget": budget_json(&d.budget),
            })
        })
        .collect();
    Json(json!({ "run_id": run, "runs": runs, "events": events })).into_response()
}

// --- Single runs: state, pause / resume / kill ---

async fn pause(
    State(st): State<Arc<AppState>>,
    Path((org, run)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    if let Err(r) = authorize(&st, &headers, &org, &run) {
        return r;
    }
    match st.ledger.run_state(&org, &run).await {
        Ok(Some(s)) if s == "killed" => return err(StatusCode::CONFLICT, "run is killed"),
        Err(e) => return unavailable(e),
        _ => {}
    }
    match st.ledger.set_run_state(&org, &run, "paused").await {
        Ok(()) => state_json(&org, &run, "paused"),
        Err(e) => unavailable(e),
    }
}

async fn resume(
    State(st): State<Arc<AppState>>,
    Path((org, run)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    if let Err(r) = authorize(&st, &headers, &org, &run) {
        return r;
    }
    match st.ledger.run_state(&org, &run).await {
        Ok(Some(s)) if s == "killed" => {
            return err(StatusCode::CONFLICT, "run is killed and cannot be resumed");
        }
        Err(e) => return unavailable(e),
        _ => {}
    }
    match st.ledger.clear_run_state(&org, &run).await {
        Ok(()) => state_json(&org, &run, "running"),
        Err(e) => unavailable(e),
    }
}

async fn kill(
    State(st): State<Arc<AppState>>,
    Path((org, run)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    if let Err(r) = authorize(&st, &headers, &org, &run) {
        return r;
    }
    match st.ledger.set_run_state(&org, &run, "killed").await {
        Ok(()) => state_json(&org, &run, "killed"),
        Err(e) => unavailable(e),
    }
}

async fn get_run(
    State(st): State<Arc<AppState>>,
    Path((org, run)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    if let Err(r) = authorize(&st, &headers, &org, &run) {
        return r;
    }
    let state = match st.ledger.run_state(&org, &run).await {
        Ok(s) => s.unwrap_or_else(|| "running".into()),
        Err(e) => return unavailable(e),
    };
    let parent = match st.ledger.run_parent(&org, &run).await {
        Ok(p) => p,
        Err(e) => return unavailable(e),
    };
    let budget = match st.ledger.budget(&Keys::new(&org).run_budget(&run)).await {
        Ok(b) => b,
        Err(e) => return unavailable(e),
    };
    Json(json!({
        "org": org,
        "run_id": run,
        "state": state,
        "parent_run_id": parent,
        "budget": budget_json(&budget),
    }))
    .into_response()
}
