use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::header::AUTHORIZATION;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::json;
use sha2::{Digest, Sha256};

use crate::config::valid_id;
use crate::ledger::Keys;
use crate::proxy::AppState;

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route("/readyz", get(readyz))
        .route("/admin/orgs/{org}/runs/{run}", get(get_run))
        .route("/admin/orgs/{org}/runs/{run}/pause", post(pause))
        .route("/admin/orgs/{org}/runs/{run}/resume", post(resume))
        .route("/admin/orgs/{org}/runs/{run}/kill", post(kill))
        .with_state(state)
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
fn authorize(st: &AppState, headers: &HeaderMap, org: &str, run: &str) -> Result<(), Response> {
    let Some(expected) = st.rt.admin_token.as_deref() else {
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
    if !st.rt.cfg.orgs.iter().any(|o| o.id == org) {
        return Err(err(StatusCode::NOT_FOUND, "unknown org"));
    }
    if !valid_id(run) {
        return Err(err(StatusCode::BAD_REQUEST, "invalid run id"));
    }
    Ok(())
}

fn unavailable(e: anyhow::Error) -> Response {
    tracing::error!(error = %e, "admin ledger call failed");
    err(StatusCode::SERVICE_UNAVAILABLE, "budget ledger unavailable")
}

fn state_json(org: &str, run: &str, state: &str) -> Response {
    Json(json!({ "org": org, "run_id": run, "state": state })).into_response()
}

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
    let micros = |field: &str| budget.get(field).and_then(|v| v.parse::<i64>().ok());
    Json(json!({
        "org": org,
        "run_id": run,
        "state": state,
        "parent_run_id": parent,
        "budget": {
            "mode": budget.get("mode"),
            "limit_micros": micros("limit").filter(|l| *l >= 0),
            "spent_micros": micros("spent").unwrap_or(0),
            "reserved_micros": micros("reserved").unwrap_or(0),
        }
    }))
    .into_response()
}
