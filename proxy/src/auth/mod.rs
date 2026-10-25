//! Who is calling the admin API, and what they may do.
//!
//! Handlers take an [`Admin`] argument: extracting it authenticates the caller, so a handler
//! that has one knows who is acting (for the audit log) and checks their role with
//! [`Admin::org`] / [`Admin::instance`]. Callers authenticate with one of:
//!
//! - a dashboard session cookie (password or single sign-on), plus an `x-csrf-token` header on
//!   anything that changes state;
//! - a personal API token (`Authorization: Bearer tlu_…`), acting as its user;
//! - the break-glass admin token (`Authorization: Bearer $TL_ADMIN_TOKEN`), acting as an
//!   instance admin. Unset it once people can sign in.
//!
//! Users, sessions and tokens live in Postgres; without a database only the admin token works.

pub mod oidc;
pub mod password;
pub mod routes;
pub mod store;

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use axum::extract::{ConnectInfo, FromRequestParts};
use axum::http::header::{AUTHORIZATION, COOKIE};
use axum::http::request::Parts;
use axum::http::{HeaderMap, Method, StatusCode};
use axum::response::Response;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::admin::err;
use crate::audit::{Change, Entry};
use crate::config::valid_id;
use crate::proxy::AppState;

pub const SESSION_COOKIE: &str = "tl_session";
pub const CSRF_HEADER: &str = "x-csrf-token";
/// Prefix of personal API tokens.
pub const API_TOKEN_PREFIX: &str = "tlu_";

/// A person's role in an org. Each role can do everything the ones below it can.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    /// Sees runs, spend, activity and settings.
    Viewer,
    /// Also pauses, resumes and kills runs (and decides approvals).
    Approver,
    /// Also changes projects, keys, limits, provider keys and members (up to admin).
    Admin,
    /// Also makes owners and deletes the org.
    Owner,
}

impl Role {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "viewer" => Some(Self::Viewer),
            "approver" => Some(Self::Approver),
            "admin" => Some(Self::Admin),
            "owner" => Some(Self::Owner),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Viewer => "viewer",
            Self::Approver => "approver",
            Self::Admin => "admin",
            Self::Owner => "owner",
        }
    }
}

/// How the caller signed in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Via {
    AdminToken,
    ApiToken,
    Session,
}

/// An authenticated admin API caller.
#[derive(Debug, Clone)]
pub struct Admin {
    /// Shown in the audit log: the user's email, or "admin-token".
    pub actor: String,
    /// The caller's address, when the server was started with connection info.
    pub ip: Option<String>,
    pub user_id: Option<i64>,
    pub instance_admin: bool,
    pub roles: HashMap<String, Role>,
    pub via: Via,
    /// The session's CSRF token (sessions only), returned by `/auth/me`.
    pub csrf: Option<String>,
    /// Hash of the session cookie (sessions only), for signing out.
    pub session_hash: Option<String>,
}

impl FromRequestParts<Arc<AppState>> for Admin {
    type Rejection = Response;

    async fn from_request_parts(
        parts: &mut Parts,
        st: &Arc<AppState>,
    ) -> Result<Self, Self::Rejection> {
        let ip = client_ip(parts);
        if let Some(bearer) = bearer(&parts.headers) {
            if bearer.starts_with(API_TOKEN_PREFIX)
                && let Some(db) = &st.db
            {
                return match store::api_token_user(db.pool(), &sha256_hex(bearer)).await {
                    Ok(Some(user)) => Ok(Self::user(user, Via::ApiToken, ip, None, None)),
                    Ok(None) => Err(err(
                        StatusCode::UNAUTHORIZED,
                        "invalid or expired API token",
                    )),
                    Err(e) => Err(db_unavailable(e)),
                };
            }
            return match admin_token_matches(st, bearer) {
                Some(true) => Ok(Self {
                    actor: "admin-token".into(),
                    ip,
                    user_id: None,
                    instance_admin: true,
                    roles: HashMap::new(),
                    via: Via::AdminToken,
                    csrf: None,
                    session_hash: None,
                }),
                Some(false) => Err(err(StatusCode::UNAUTHORIZED, "invalid admin token")),
                None => Err(err(
                    StatusCode::UNAUTHORIZED,
                    "the admin token is disabled on this proxy; sign in instead",
                )),
            };
        }
        let (Some(cookie), Some(db)) = (cookie(&parts.headers, SESSION_COOKIE), &st.db) else {
            return Err(err(StatusCode::UNAUTHORIZED, "sign in first"));
        };
        let hash = sha256_hex(cookie);
        let idle = st.rt().cfg.auth.idle_minutes;
        let session = match store::session_user(db.pool(), &hash, idle).await {
            Ok(Some(s)) => s,
            Ok(None) => {
                return Err(err(
                    StatusCode::UNAUTHORIZED,
                    "your session has ended; sign in again",
                ));
            }
            Err(e) => return Err(db_unavailable(e)),
        };
        // Cookies ride along on cross-site form posts too; a header can only come from our page.
        if !matches!(parts.method, Method::GET | Method::HEAD | Method::OPTIONS) {
            let given = parts
                .headers
                .get(CSRF_HEADER)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("");
            if !constant_time_eq(given, &session.csrf) {
                return Err(err(
                    StatusCode::FORBIDDEN,
                    "missing or wrong x-csrf-token header",
                ));
            }
        }
        let csrf = session.csrf.clone();
        Ok(Self::user(
            session.user,
            Via::Session,
            ip,
            Some(csrf),
            Some(hash),
        ))
    }
}

impl Admin {
    fn user(
        u: store::AuthUser,
        via: Via,
        ip: Option<String>,
        csrf: Option<String>,
        session_hash: Option<String>,
    ) -> Self {
        Self {
            actor: u.email,
            ip,
            user_id: Some(u.id),
            instance_admin: u.instance_admin,
            roles: u.roles,
            via,
            csrf,
            session_hash,
        }
    }

    /// The caller's role in `org` (instance admins are owners everywhere).
    pub fn role_in(&self, org: &str) -> Option<Role> {
        if self.instance_admin {
            return Some(Role::Owner);
        }
        self.roles.get(org).copied()
    }

    pub fn can_see(&self, org: &str) -> bool {
        self.role_in(org).is_some()
    }

    /// May this caller do something that needs `need` in `org`? Orgs the caller is not a member
    /// of look like they don't exist.
    #[allow(clippy::result_large_err)]
    pub fn org(&self, st: &AppState, org: &str, need: Role) -> Result<(), Response> {
        let exists = st.rt().cfg.orgs.iter().any(|o| o.id == org);
        match self.role_in(org) {
            Some(r) if exists && r >= need => Ok(()),
            Some(r) if exists => Err(err(
                StatusCode::FORBIDDEN,
                &format!(
                    "this needs the {} role in org {org}; you are {}",
                    need.as_str(),
                    r.as_str()
                ),
            )),
            _ => Err(err(StatusCode::NOT_FOUND, "unknown org")),
        }
    }

    /// Like [`Admin::org`], for a run in it.
    #[allow(clippy::result_large_err)]
    pub fn run(&self, st: &AppState, org: &str, run: &str, need: Role) -> Result<(), Response> {
        self.org(st, org, need)?;
        if !valid_id(run) {
            return Err(err(StatusCode::BAD_REQUEST, "invalid run id"));
        }
        Ok(())
    }

    /// Install-wide settings (model prices, creating orgs, users, single sign-on).
    #[allow(clippy::result_large_err)]
    pub fn instance(&self) -> Result<(), Response> {
        if self.instance_admin {
            Ok(())
        } else {
            Err(err(
                StatusCode::FORBIDDEN,
                "only instance admins can do this",
            ))
        }
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
        let mut e = self.entry_global(action, target, detail);
        e.org = Some(org.to_string());
        e
    }

    /// An audit entry that is not about one org (users, sign-in, single sign-on).
    pub fn entry_global(&self, action: &str, target: &str, detail: serde_json::Value) -> Entry {
        Entry {
            org: None,
            actor: self.actor.clone(),
            action: action.to_string(),
            target: target.to_string(),
            detail,
            ip: self.ip.clone(),
        }
    }
}

/// `Some(true/false)` when the admin token is enabled; None when it is not set.
fn admin_token_matches(st: &AppState, given: &str) -> Option<bool> {
    let rt = st.rt();
    let expected = rt.admin_token.as_deref()?;
    Some(constant_time_eq(given, expected))
}

/// Compare digests so the check takes the same time regardless of where the strings differ.
pub fn constant_time_eq(a: &str, b: &str) -> bool {
    let (a, b) = (Sha256::digest(a.as_bytes()), Sha256::digest(b.as_bytes()));
    a.iter()
        .zip(b.iter())
        .fold(0u8, |acc, (x, y)| acc | (x ^ y))
        == 0
}

pub fn sha256_hex(s: &str) -> String {
    hex::encode(Sha256::digest(s.as_bytes()))
}

/// A random secret: the prefix plus 244 random bits, hex.
pub fn random_token(prefix: &str) -> String {
    format!(
        "{prefix}{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    )
}

fn bearer(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::trim)
}

pub fn cookie<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers
        .get_all(COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|kv| kv.trim().split_once('='))
        .find(|(k, _)| *k == name)
        .map(|(_, v)| v)
        .filter(|v| !v.is_empty())
}

pub fn client_ip(parts: &Parts) -> Option<String> {
    parts
        .extensions
        .get::<ConnectInfo<SocketAddr>>()
        .map(|c| c.0.ip().to_string())
}

pub fn db_unavailable(e: anyhow::Error) -> Response {
    tracing::error!(error = %e, "auth database call failed");
    err(StatusCode::SERVICE_UNAVAILABLE, "user database unavailable")
}

/// In-process state for sign-in: attempt counters and the identity provider's metadata.
#[derive(Default)]
pub struct AuthState {
    attempts: Mutex<HashMap<String, (u64, u32)>>,
    pub(crate) sso: tokio::sync::Mutex<Option<oidc::Provider>>,
}

impl AuthState {
    /// Allow at most `max` attempts per `window_secs` for `key` (e.g. a client address).
    pub fn allow(&self, key: &str, max: u32, window_secs: u64) -> bool {
        let now = crate::ledger::unix_now().max(0) as u64;
        let mut map = self.attempts.lock().unwrap_or_else(|e| e.into_inner());
        if map.len() > 10_000 {
            map.retain(|_, (start, _)| now < *start + window_secs);
        }
        let entry = map.entry(key.to_string()).or_insert((now, 0));
        if now >= entry.0 + window_secs {
            *entry = (now, 0);
        }
        entry.1 += 1;
        entry.1 <= max
    }
}
