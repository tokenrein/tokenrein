//! Who is calling the admin API.
//!
//! Handlers take an [`Admin`] argument: extracting it checks the caller's credentials (the admin
//! token for now), so a handler that has one may act, and it names the caller for the audit
//! log.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::extract::{ConnectInfo, FromRequestParts};
use axum::http::header::AUTHORIZATION;
use axum::http::request::Parts;
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use sha2::{Digest, Sha256};

use crate::admin::err;
use crate::audit::{Change, Entry};
use crate::config::valid_id;
use crate::proxy::AppState;

/// An authenticated admin API caller.
#[derive(Debug, Clone)]
pub struct Admin {
    /// Shown in the audit log.
    pub actor: String,
    /// The caller's address, when the server was started with connection info.
    pub ip: Option<String>,
}

impl FromRequestParts<Arc<AppState>> for Admin {
    type Rejection = Response;

    async fn from_request_parts(
        parts: &mut Parts,
        st: &Arc<AppState>,
    ) -> Result<Self, Self::Rejection> {
        check_token(st, &parts.headers)?;
        let ip = parts
            .extensions
            .get::<ConnectInfo<SocketAddr>>()
            .map(|c| c.0.ip().to_string());
        Ok(Self {
            actor: "admin-token".into(),
            ip,
        })
    }
}

impl Admin {
    /// May this caller manage `org`? (Every admin may, for now; the org must exist.)
    #[allow(clippy::result_large_err)]
    pub fn org(&self, st: &AppState, org: &str) -> Result<(), Response> {
        if !st.rt().cfg.orgs.iter().any(|o| o.id == org) {
            return Err(err(StatusCode::NOT_FOUND, "unknown org"));
        }
        Ok(())
    }

    /// Like [`Admin::org`], for a run in it.
    #[allow(clippy::result_large_err)]
    pub fn run(&self, st: &AppState, org: &str, run: &str) -> Result<(), Response> {
        self.org(st, org)?;
        if !valid_id(run) {
            return Err(err(StatusCode::BAD_REQUEST, "invalid run id"));
        }
        Ok(())
    }

    /// A config change this caller is about to make.
    pub fn change(&self, action: &'static str, org: Option<&str>, target: &str) -> Change {
        Change {
            actor: self.actor.clone(),
            ip: self.ip.clone(),
            org: org.map(str::to_string),
            action,
            target: target.to_string(),
        }
    }

    /// An audit entry for something other than a config change.
    pub fn entry(&self, action: &str, org: &str, target: &str, detail: serde_json::Value) -> Entry {
        Entry {
            org: Some(org.to_string()),
            actor: self.actor.clone(),
            action: action.to_string(),
            target: target.to_string(),
            detail,
            ip: self.ip.clone(),
        }
    }
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
