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
    Budgets, FailMode, OrgConfig, ProjectConfig, VirtualKey, generate_virtual_key, valid_id,
};
use crate::control::{self, EditError};
use crate::ledger::Keys;
use crate::proxy::AppState;

const DASHBOARD_HTML: &str = include_str!("dashboard.html");

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/", get(dashboard))
        .route("/healthz", get(|| async { "ok" }))
        .route("/readyz", get(readyz))
        .route("/admin/overview", get(overview))
        .route("/admin/orgs", post(create_org))
        .route("/admin/orgs/{org}", put(update_org).delete(delete_org))
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
        .map(|org| {
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
                        "budgets": p.budgets,
                        "effective": Budgets::effective(&org.budgets, &p.budgets),
                        "keys": keys,
                        "spend": { "project_monthly": spend.next().map(budget_json) },
                    })
                })
                .collect();
            json!({
                "id": org.id,
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
