//! HTTP endpoints for signing in and managing who has access:
//!
//! - `/auth/*`: sign in with a password or single sign-on, set a password from a setup link,
//!   sign out, see yourself, personal API tokens.
//! - `/admin/orgs/{org}/members`: an org's people and their roles.
//! - `/admin/users`, `/admin/sso`: everyone on the install, and single sign-on (instance admins).

use std::sync::Arc;

use axum::extract::{FromRequestParts, Path, Query, State};
use axum::http::header::{LOCATION, SET_COOKIE};
use axum::http::request::Parts;
use axum::http::{HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};

use super::oidc::{self, GroupRole, SsoConfig, TokenAuth};
use super::{Admin, Role, SESSION_COOKIE, Via, client_ip, db_unavailable, password, store};
use crate::admin::err;
use crate::audit::{self, Entry};
use crate::proxy::AppState;

/// Sign-in attempts per client address per 5 minutes, and per account.
const LOGINS_PER_IP: u32 = 30;
const LOGINS_PER_ACCOUNT: u32 = 10;
/// Setup links work for this long.
const SETUP_LINK_HOURS: u32 = 72;
/// Longest an API token may live.
const MAX_TOKEN_DAYS: u32 = 365;

pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/auth/config", get(auth_config))
        .route("/auth/login", post(login))
        .route("/auth/setup", post(setup))
        .route("/auth/logout", post(logout))
        .route("/auth/me", get(me))
        .route("/auth/password", put(change_password))
        .route("/auth/tokens", get(list_tokens).post(create_token))
        .route("/auth/tokens/{id}", axum::routing::delete(revoke_token))
        .route("/auth/sso/start", get(sso_start))
        .route("/auth/sso/callback", get(sso_callback))
        .route(
            "/admin/orgs/{org}/members",
            get(list_members).post(add_member),
        )
        .route(
            "/admin/orgs/{org}/members/{user}",
            put(update_member).delete(remove_member),
        )
        .route(
            "/admin/orgs/{org}/members/{user}/setup-link",
            post(member_setup_link),
        )
        .route("/admin/users", get(list_users).post(create_user))
        .route("/admin/users/{user}", put(update_user))
        .route("/admin/sso", get(get_sso).put(put_sso))
        .route("/admin/sso/test", post(test_sso))
}

/// The caller's address, for rate limits and the audit log. Never rejects.
pub struct ClientIp(pub Option<String>);

impl<S: Send + Sync> FromRequestParts<S> for ClientIp {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(parts: &mut Parts, _: &S) -> Result<Self, Self::Rejection> {
        Ok(Self(client_ip(parts)))
    }
}

#[allow(clippy::result_large_err)]
fn db(st: &AppState) -> Result<&sqlx::PgPool, Response> {
    st.db.as_ref().map(|d| d.pool()).ok_or_else(|| {
        err(
            StatusCode::CONFLICT,
            "people and sign-in need a database: set TL_DATABASE_URL on the proxy",
        )
    })
}

fn normalize_email(e: &str) -> Option<String> {
    let e = e.trim().to_lowercase();
    let (local, domain) = e.split_once('@')?;
    let ok = !local.is_empty()
        && domain.contains('.')
        && e.len() <= 254
        && !e.chars().any(|c| c.is_whitespace() || c.is_control());
    ok.then_some(e)
}

fn session_cookie(st: &AppState, value: &str, max_age_secs: u64) -> HeaderValue {
    let secure = st
        .rt()
        .cfg
        .auth
        .public_url
        .as_deref()
        .is_some_and(|u| u.starts_with("https://"));
    let cookie = format!(
        "{SESSION_COOKIE}={value}; Path=/; HttpOnly; SameSite=Lax; Max-Age={max_age_secs}{}",
        if secure { "; Secure" } else { "" }
    );
    HeaderValue::from_str(&cookie).unwrap_or_else(|_| HeaderValue::from_static(""))
}

/// Start a session and answer with its cookie.
async fn signed_in(
    st: &AppState,
    user_id: i64,
    via: &str,
    ip: Option<&str>,
    body: Value,
) -> Response {
    let pool = match db(st) {
        Ok(p) => p,
        Err(r) => return r,
    };
    let hours = st.rt().cfg.auth.session_hours;
    let (token, csrf) = match store::create_session(pool, user_id, via, ip, hours).await {
        Ok(t) => t,
        Err(e) => return db_unavailable(e),
    };
    if let Err(e) = store::login_succeeded(pool, user_id).await {
        return db_unavailable(e);
    }
    let mut body = body;
    body["csrf"] = json!(csrf);
    let mut resp = Json(body).into_response();
    resp.headers_mut().insert(
        SET_COOKIE,
        session_cookie(st, &token, u64::from(hours) * 3600),
    );
    resp
}

fn anon(action: &str, who: &str, ip: Option<String>, detail: Value) -> Entry {
    Entry {
        org: None,
        actor: who.to_string(),
        action: action.to_string(),
        target: who.to_string(),
        detail,
        ip,
    }
}

// --- Sign-in ---

/// How people can sign in on this install (shown on the sign-in page; no secrets).
async fn auth_config(State(st): State<Arc<AppState>>) -> Response {
    let rt = st.rt();
    let mut out = json!({
        "database": st.db.is_some(),
        "password_login": st.db.is_some() && rt.cfg.auth.password_login,
        "admin_token": rt.admin_token.is_some(),
        "sso": null,
    });
    if let Some(pool) = st.db.as_ref().map(|d| d.pool()) {
        match store::count_users(pool).await {
            Ok(n) => out["users"] = json!(n),
            Err(e) => return db_unavailable(e),
        }
        match SsoConfig::load(&st).await {
            Ok(Some(c)) if c.enabled => {
                let label = if c.label.is_empty() {
                    "single sign-on".to_string()
                } else {
                    c.label
                };
                out["sso"] = json!({ "label": label });
            }
            Ok(_) => {}
            Err(e) => return db_unavailable(e),
        }
    }
    Json(out).into_response()
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LoginBody {
    email: String,
    password: String,
}

async fn login(
    State(st): State<Arc<AppState>>,
    ClientIp(ip): ClientIp,
    Json(body): Json<LoginBody>,
) -> Response {
    let pool = match db(&st) {
        Ok(p) => p,
        Err(r) => return r,
    };
    if !st.rt().cfg.auth.password_login {
        return err(
            StatusCode::FORBIDDEN,
            "password sign-in is turned off here; use single sign-on",
        );
    }
    let email = body.email.trim().to_lowercase();
    let ip_key = format!("login-ip:{}", ip.as_deref().unwrap_or("?"));
    if !st.auth.allow(&ip_key, LOGINS_PER_IP, 300)
        || !st
            .auth
            .allow(&format!("login:{email}"), LOGINS_PER_ACCOUNT, 300)
    {
        return err(
            StatusCode::TOO_MANY_REQUESTS,
            "too many sign-in attempts; wait a few minutes",
        );
    }
    let user = match store::user_for_login(pool, &email).await {
        Ok(u) => u,
        Err(e) => return db_unavailable(e),
    };
    let stored = user.as_ref().and_then(|u| u.password_hash.clone());
    let ok = match password::verify(body.password, stored).await {
        Ok(ok) => ok,
        Err(e) => return db_unavailable(e),
    };
    let refuse = |reason: &str| {
        anon(
            "auth.login_failed",
            &email,
            ip.clone(),
            json!({ "reason": reason }),
        )
    };
    match user {
        Some(u) if u.locked => {
            audit::record(&st, refuse("locked")).await;
            err(
                StatusCode::TOO_MANY_REQUESTS,
                "too many failed sign-ins for this account; try again in 15 minutes",
            )
        }
        Some(u) if ok && !u.disabled => {
            let entry = anon(
                "auth.login",
                &u.email,
                ip.clone(),
                json!({ "via": "password" }),
            );
            audit::record(&st, entry).await;
            let body = json!({ "email": u.email });
            signed_in(&st, u.id, "password", ip.as_deref(), body).await
        }
        Some(u) => {
            if !ok && let Err(e) = store::login_failed(pool, u.id).await {
                return db_unavailable(e);
            }
            let reason = if u.disabled {
                "disabled"
            } else {
                "wrong password"
            };
            audit::record(&st, refuse(reason)).await;
            err(StatusCode::UNAUTHORIZED, "wrong email or password")
        }
        None => {
            audit::record(&st, refuse("unknown account")).await;
            err(StatusCode::UNAUTHORIZED, "wrong email or password")
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SetupBody {
    token: String,
    password: String,
    name: Option<String>,
}

/// Set a password from a setup link (new account or reset), and sign in.
async fn setup(
    State(st): State<Arc<AppState>>,
    ClientIp(ip): ClientIp,
    Json(body): Json<SetupBody>,
) -> Response {
    let pool = match db(&st) {
        Ok(p) => p,
        Err(r) => return r,
    };
    let ip_key = format!("setup-ip:{}", ip.as_deref().unwrap_or("?"));
    if !st.auth.allow(&ip_key, LOGINS_PER_IP, 300) {
        return err(
            StatusCode::TOO_MANY_REQUESTS,
            "too many attempts; wait a few minutes",
        );
    }
    let token = body.token.trim();
    let email = match store::peek_setup_token(pool, token).await {
        Ok(Some((_, email))) => email,
        Ok(None) => {
            return err(
                StatusCode::BAD_REQUEST,
                "this setup link expired or was already used; ask an admin for a new one",
            );
        }
        Err(e) => return db_unavailable(e),
    };
    if let Some(why) = password::weakness(&body.password, &email) {
        return err(
            StatusCode::BAD_REQUEST,
            &format!("choose another password: {why}"),
        );
    }
    let hash = match password::hash(body.password).await {
        Ok(h) => h,
        Err(e) => return db_unavailable(e),
    };
    let user_id = match store::take_setup_token(pool, token).await {
        Ok(Some((id, _))) => id,
        Ok(None) => return err(StatusCode::BAD_REQUEST, "this setup link was just used"),
        Err(e) => return db_unavailable(e),
    };
    let name: String = body
        .name
        .as_deref()
        .unwrap_or("")
        .trim()
        .chars()
        .take(200)
        .collect();
    let result = async {
        store::set_password(pool, user_id, &hash).await?;
        if !name.is_empty() {
            store::set_name_if_empty(pool, user_id, &name).await?;
        }
        // A new password ends every other session.
        store::delete_user_sessions(pool, user_id).await
    }
    .await;
    if let Err(e) = result {
        return db_unavailable(e);
    }
    audit::record(
        &st,
        anon("auth.password_set", &email, ip.clone(), json!({})),
    )
    .await;
    let body = json!({ "email": email });
    signed_in(&st, user_id, "password", ip.as_deref(), body).await
}

async fn logout(State(st): State<Arc<AppState>>, admin: Admin) -> Response {
    if let (Some(hash), Some(db)) = (&admin.session_hash, &st.db)
        && let Err(e) = store::delete_session(db.pool(), hash).await
    {
        return db_unavailable(e);
    }
    audit::record(
        &st,
        admin.entry_global("auth.logout", &admin.actor, json!({})),
    )
    .await;
    let mut resp = StatusCode::NO_CONTENT.into_response();
    resp.headers_mut()
        .insert(SET_COOKIE, session_cookie(&st, "", 0));
    resp
}

async fn me(admin: Admin) -> Response {
    let roles: serde_json::Map<String, Value> = admin
        .roles
        .iter()
        .map(|(o, r)| (o.clone(), json!(r)))
        .collect();
    let via = match admin.via {
        Via::AdminToken => "admin_token",
        Via::ApiToken => "api_token",
        Via::Session => "session",
    };
    Json(json!({
        "email": admin.actor,
        "user_id": admin.user_id,
        "instance_admin": admin.instance_admin,
        "roles": roles,
        "via": via,
        "csrf": admin.csrf,
    }))
    .into_response()
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PasswordBody {
    current: Option<String>,
    new: String,
}

async fn change_password(
    State(st): State<Arc<AppState>>,
    admin: Admin,
    Json(body): Json<PasswordBody>,
) -> Response {
    let pool = match db(&st) {
        Ok(p) => p,
        Err(r) => return r,
    };
    let (Some(user_id), Via::Session) = (admin.user_id, admin.via) else {
        return err(
            StatusCode::BAD_REQUEST,
            "sign in to the dashboard to change your password",
        );
    };
    if !st
        .auth
        .allow(&format!("password:{user_id}"), LOGINS_PER_ACCOUNT, 300)
    {
        return err(
            StatusCode::TOO_MANY_REQUESTS,
            "too many attempts; wait a few minutes",
        );
    }
    let stored = match store::password_hash(pool, user_id).await {
        Ok(h) => h,
        Err(e) => return db_unavailable(e),
    };
    if stored.is_some() {
        let ok = password::verify(body.current.unwrap_or_default(), stored)
            .await
            .unwrap_or(false);
        if !ok {
            return err(StatusCode::FORBIDDEN, "your current password is not right");
        }
    }
    if let Some(why) = password::weakness(&body.new, &admin.actor) {
        return err(
            StatusCode::BAD_REQUEST,
            &format!("choose another password: {why}"),
        );
    }
    let hash = match password::hash(body.new).await {
        Ok(h) => h,
        Err(e) => return db_unavailable(e),
    };
    let keep = admin.session_hash.clone().unwrap_or_default();
    let result = async {
        store::set_password(pool, user_id, &hash).await?;
        store::delete_other_sessions(pool, user_id, &keep).await
    }
    .await;
    if let Err(e) = result {
        return db_unavailable(e);
    }
    let entry = admin.entry_global("auth.password_changed", &admin.actor, json!({}));
    audit::record(&st, entry).await;
    StatusCode::NO_CONTENT.into_response()
}

// --- Personal API tokens ---

#[allow(clippy::result_large_err)]
fn person(admin: &Admin) -> Result<i64, Response> {
    admin.user_id.ok_or_else(|| {
        err(
            StatusCode::BAD_REQUEST,
            "API tokens belong to a person: sign in with your account to manage them",
        )
    })
}

async fn list_tokens(State(st): State<Arc<AppState>>, admin: Admin) -> Response {
    let (pool, user_id) = match (db(&st), person(&admin)) {
        (Ok(p), Ok(u)) => (p, u),
        (Err(r), _) | (_, Err(r)) => return r,
    };
    match store::list_api_tokens(pool, user_id).await {
        Ok(tokens) => Json(json!({ "tokens": tokens })).into_response(),
        Err(e) => db_unavailable(e),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TokenBody {
    name: String,
    expires_days: Option<u32>,
}

async fn create_token(
    State(st): State<Arc<AppState>>,
    admin: Admin,
    Json(body): Json<TokenBody>,
) -> Response {
    let (pool, user_id) = match (db(&st), person(&admin)) {
        (Ok(p), Ok(u)) => (p, u),
        (Err(r), _) | (_, Err(r)) => return r,
    };
    let name = body.name.trim();
    if name.is_empty() || name.chars().count() > 100 {
        return err(
            StatusCode::BAD_REQUEST,
            "token name must be 1-100 characters",
        );
    }
    let days = body.expires_days.unwrap_or(90);
    if days == 0 || days > MAX_TOKEN_DAYS {
        return err(
            StatusCode::BAD_REQUEST,
            &format!("tokens expire after 1-{MAX_TOKEN_DAYS} days"),
        );
    }
    match store::create_api_token(pool, user_id, name, Some(days)).await {
        Ok((id, token, prefix)) => {
            let detail = json!({ "name": name, "prefix": prefix, "expires_days": days });
            let entry = admin.entry_global("token.create", &id.to_string(), detail);
            audit::record(&st, entry).await;
            let body = json!({ "id": id, "name": name, "prefix": prefix, "token": token });
            (StatusCode::CREATED, Json(body)).into_response()
        }
        Err(e) => db_unavailable(e),
    }
}

async fn revoke_token(
    State(st): State<Arc<AppState>>,
    Path(id): Path<i64>,
    admin: Admin,
) -> Response {
    let (pool, user_id) = match (db(&st), person(&admin)) {
        (Ok(p), Ok(u)) => (p, u),
        (Err(r), _) | (_, Err(r)) => return r,
    };
    match store::revoke_api_token(pool, user_id, id).await {
        Ok(true) => {
            let entry = admin.entry_global("token.revoke", &id.to_string(), json!({}));
            audit::record(&st, entry).await;
            StatusCode::NO_CONTENT.into_response()
        }
        Ok(false) => err(StatusCode::NOT_FOUND, "unknown token"),
        Err(e) => db_unavailable(e),
    }
}

// --- Single sign-on ---

/// Back to the dashboard, with an error to show on the sign-in page.
fn to_login(message: &str) -> Response {
    let msg: String = url::form_urlencoded::byte_serialize(message.as_bytes()).collect();
    redirect(&format!("/#login-error={msg}"))
}

fn redirect(to: &str) -> Response {
    let mut resp = StatusCode::FOUND.into_response();
    if let Ok(v) = HeaderValue::from_str(to) {
        resp.headers_mut().insert(LOCATION, v);
    }
    resp
}

async fn sso_start(State(st): State<Arc<AppState>>, ClientIp(ip): ClientIp) -> Response {
    let key = format!("sso-ip:{}", ip.as_deref().unwrap_or("?"));
    if !st.auth.allow(&key, LOGINS_PER_IP, 300) {
        return to_login("too many sign-in attempts; wait a few minutes");
    }
    let cfg = match SsoConfig::load(&st).await {
        Ok(Some(c)) if c.enabled => c,
        Ok(_) => return to_login("single sign-on is not set up here"),
        Err(e) => return db_unavailable(e),
    };
    match oidc::authorize_url(&st, &cfg).await {
        Ok(url) => redirect(&url),
        Err(e) => {
            tracing::error!(error = %format!("{e:#}"), "could not start single sign-on");
            to_login("single sign-on is not working right now; ask an admin to check its settings")
        }
    }
}

#[derive(Deserialize)]
struct CallbackQuery {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
    error_description: Option<String>,
}

async fn sso_callback(
    State(st): State<Arc<AppState>>,
    ClientIp(ip): ClientIp,
    Query(q): Query<CallbackQuery>,
) -> Response {
    if let Some(e) = q.error {
        let detail: String = q.error_description.unwrap_or(e).chars().take(200).collect();
        return to_login(&format!("the identity provider said: {detail}"));
    }
    let (Some(code), Some(state)) = (q.code, q.state) else {
        return to_login("the sign-in was incomplete; start again");
    };
    let cfg = match SsoConfig::load(&st).await {
        Ok(Some(c)) if c.enabled => c,
        Ok(_) => return to_login("single sign-on is not set up here"),
        Err(e) => return db_unavailable(e),
    };
    let result = async {
        let id = oidc::finish(&st, &cfg, &code, &state).await?;
        let (user_id, created) = oidc::provision(&st, &cfg, &id).await?;
        anyhow::Ok((id, user_id, created))
    }
    .await;
    let (id, user_id, created) = match result {
        Ok(r) => r,
        Err(e) => {
            let detail = json!({ "reason": format!("{e:#}") });
            audit::record(&st, anon("auth.sso_failed", "unknown", ip.clone(), detail)).await;
            return to_login(&e.to_string());
        }
    };
    let detail = json!({ "via": "sso", "created": created, "groups": id.groups.len() });
    audit::record(&st, anon("auth.login", &id.email, ip.clone(), detail)).await;
    let resp = signed_in(&st, user_id, "sso", ip.as_deref(), json!({})).await;
    if !resp.status().is_success() {
        return resp;
    }
    let mut out = redirect("/");
    if let Some(c) = resp.headers().get(SET_COOKIE) {
        out.headers_mut().insert(SET_COOKIE, c.clone());
    }
    out
}

async fn get_sso(State(st): State<Arc<AppState>>, admin: Admin) -> Response {
    if let Err(r) = admin.instance() {
        return r;
    }
    if let Err(r) = db(&st) {
        return r;
    }
    let cfg = match SsoConfig::load(&st).await {
        Ok(c) => c.unwrap_or_default(),
        Err(e) => return db_unavailable(e),
    };
    let rt = st.rt();
    Json(json!({
        "config": cfg.public(),
        "redirect_uri": oidc::redirect_uri(&st),
        "secrets_enabled": rt.secrets().is_some(),
        "password_login": rt.cfg.auth.password_login,
    }))
    .into_response()
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SsoBody {
    enabled: bool,
    #[serde(default)]
    label: String,
    #[serde(default)]
    issuer: String,
    #[serde(default)]
    client_id: String,
    /// A new client secret; leave out to keep the saved one, "" to remove it.
    client_secret: Option<String>,
    client_secret_env: Option<String>,
    #[serde(default)]
    token_auth: TokenAuth,
    #[serde(default)]
    allowed_domains: Vec<String>,
    #[serde(default)]
    auto_create: bool,
    default_org: Option<String>,
    default_role: Option<Role>,
    #[serde(default)]
    groups_claim: String,
    #[serde(default)]
    group_roles: Vec<GroupRole>,
}

fn valid_issuer(u: &str) -> bool {
    match url::Url::parse(u) {
        Ok(url) => {
            url.scheme() == "https"
                || (url.scheme() == "http"
                    && matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "[::1]")))
        }
        Err(_) => false,
    }
}

async fn put_sso(
    State(st): State<Arc<AppState>>,
    admin: Admin,
    Json(b): Json<SsoBody>,
) -> Response {
    if let Err(r) = admin.instance() {
        return r;
    }
    let pool = match db(&st) {
        Ok(p) => p,
        Err(r) => return r,
    };
    let before = match SsoConfig::load(&st).await {
        Ok(c) => c.unwrap_or_default(),
        Err(e) => return db_unavailable(e),
    };
    let issuer = b.issuer.trim().trim_end_matches('/').to_string();
    if b.enabled {
        if !valid_issuer(&issuer) {
            return err(
                StatusCode::BAD_REQUEST,
                "the issuer must be an https:// URL",
            );
        }
        if b.client_id.trim().is_empty() {
            return err(StatusCode::BAD_REQUEST, "the client ID is required");
        }
        if st.rt().cfg.auth.public_url.is_none() {
            return err(
                StatusCode::CONFLICT,
                "set auth.public_url in the proxy config (the dashboard's address) first; \
                 the identity provider sends people back there",
            );
        }
    }
    let orgs: Vec<String> = st.rt().cfg.orgs.iter().map(|o| o.id.clone()).collect();
    for m in &b.group_roles {
        if m.group.trim().is_empty() || !orgs.contains(&m.org) {
            return err(
                StatusCode::BAD_REQUEST,
                &format!(
                    "group mapping {:?} -> {:?}: unknown org or empty group",
                    m.group, m.org
                ),
            );
        }
    }
    if let Some(o) = b.default_org.as_deref().filter(|o| !o.is_empty())
        && !orgs.iter().any(|x| x == o)
    {
        return err(StatusCode::BAD_REQUEST, "the default org does not exist");
    }
    let client_secret = match b.client_secret.as_deref().map(str::trim) {
        None => before.client_secret.clone(),
        Some("") => None,
        Some(s) => match SsoConfig::seal_secret(&st, s) {
            Ok(sealed) => Some(sealed),
            Err(e) => return err(StatusCode::CONFLICT, &e.to_string()),
        },
    };
    let cfg = SsoConfig {
        enabled: b.enabled,
        label: b.label.trim().chars().take(40).collect(),
        issuer,
        client_id: b.client_id.trim().to_string(),
        client_secret,
        client_secret_env: b.client_secret_env.filter(|s| !s.trim().is_empty()),
        token_auth: b.token_auth,
        allowed_domains: b
            .allowed_domains
            .iter()
            .map(|d| d.trim().trim_start_matches('@').to_lowercase())
            .filter(|d| !d.is_empty())
            .collect(),
        auto_create: b.auto_create,
        default_org: b.default_org.filter(|o| !o.is_empty()),
        default_role: b.default_role,
        groups_claim: b.groups_claim.trim().to_string(),
        group_roles: b.group_roles,
    };
    let stored = match serde_json::to_value(&cfg) {
        Ok(v) => v,
        Err(e) => return db_unavailable(e.into()),
    };
    if let Err(e) = store::put_sso_config(pool, &stored).await {
        return db_unavailable(e);
    }
    *st.auth.sso.lock().await = None;
    let changes = audit::diff(&before.public(), &cfg.public());
    let entry = admin.entry_global(
        "sso.update",
        "single sign-on",
        json!({ "changes": changes }),
    );
    audit::record(&st, entry).await;
    Json(json!({ "config": cfg.public() })).into_response()
}

/// Check that the issuer answers with valid OpenID metadata and keys.
async fn test_sso(State(st): State<Arc<AppState>>, admin: Admin) -> Response {
    if let Err(r) = admin.instance() {
        return r;
    }
    let cfg = match SsoConfig::load(&st).await {
        Ok(Some(c)) if !c.issuer.is_empty() => c,
        Ok(_) => {
            return Json(json!({ "ok": false, "message": "save an issuer first" })).into_response();
        }
        Err(e) => return db_unavailable(e),
    };
    match oidc::provider(&st, &cfg, true).await {
        Ok(p) => Json(json!({
            "ok": true,
            "message": "the identity provider answered with valid metadata and signing keys",
            "authorization_endpoint": p.authorization_endpoint,
        }))
        .into_response(),
        Err(e) => Json(json!({ "ok": false, "message": format!("{e:#}") })).into_response(),
    }
}

// --- Org members ---

async fn list_members(
    State(st): State<Arc<AppState>>,
    Path(org): Path<String>,
    admin: Admin,
) -> Response {
    if let Err(r) = admin.org(&st, &org, Role::Viewer) {
        return r;
    }
    let pool = match db(&st) {
        Ok(p) => p,
        Err(r) => return r,
    };
    match store::members(pool, &org).await {
        Ok(members) => Json(json!({ "members": members })).into_response(),
        Err(e) => db_unavailable(e),
    }
}

/// May `admin` give or take away `role` (the target's current or new role) in `org`?
#[allow(clippy::result_large_err)]
fn may_manage(admin: &Admin, org: &str, role: Role) -> Result<(), Response> {
    if role == Role::Owner && admin.role_in(org) != Some(Role::Owner) {
        return Err(err(
            StatusCode::FORBIDDEN,
            "only owners can make or change owners",
        ));
    }
    Ok(())
}

fn setup_link(st: &AppState, token: &str) -> Value {
    let path = format!("/#setup={token}");
    let url = st
        .rt()
        .cfg
        .auth
        .public_url
        .as_deref()
        .map(|u| format!("{}{path}", u.trim_end_matches('/')));
    json!({ "setup_path": path, "setup_url": url })
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MemberBody {
    email: String,
    role: Role,
    name: Option<String>,
}

async fn add_member(
    State(st): State<Arc<AppState>>,
    Path(org): Path<String>,
    admin: Admin,
    Json(b): Json<MemberBody>,
) -> Response {
    if let Err(r) = admin.org(&st, &org, Role::Admin) {
        return r;
    }
    if let Err(r) = may_manage(&admin, &org, b.role) {
        return r;
    }
    let pool = match db(&st) {
        Ok(p) => p,
        Err(r) => return r,
    };
    let Some(email) = normalize_email(&b.email) else {
        return err(StatusCode::BAD_REQUEST, "enter a valid email address");
    };
    let name: String = b
        .name
        .as_deref()
        .unwrap_or("")
        .trim()
        .chars()
        .take(200)
        .collect();
    let result = async {
        let (user_id, created) = store::upsert_user(pool, &email, &name).await?;
        let current = store::role_of(pool, user_id, &org).await?;
        let user = store::user(pool, user_id).await?;
        anyhow::Ok((user_id, created, current, user))
    }
    .await;
    let (user_id, created, current, user) = match result {
        Ok(r) => r,
        Err(e) => return db_unavailable(e),
    };
    if user.is_some_and(|u| u.3) {
        return err(
            StatusCode::CONFLICT,
            "that account is disabled; an instance admin can enable it",
        );
    }
    if let Some(cur) = current {
        if let Err(r) = may_manage(&admin, &org, cur) {
            return r;
        }
        if let Err(r) = last_owner_guard(pool, &org, cur, Some(b.role)).await {
            return r;
        }
    }
    if let Err(e) = store::set_role(pool, user_id, &org, b.role).await {
        return db_unavailable(e);
    }
    let mut out = json!({ "user_id": user_id, "email": email, "role": b.role, "created": created });
    let has_password = match store::password_hash(pool, user_id).await {
        Ok(h) => h.is_some(),
        Err(e) => return db_unavailable(e),
    };
    if !has_password && st.rt().cfg.auth.password_login {
        match store::create_setup_token(pool, user_id, SETUP_LINK_HOURS).await {
            Ok(token) => {
                let link = setup_link(&st, &token);
                out["setup_path"] = link["setup_path"].clone();
                out["setup_url"] = link["setup_url"].clone();
            }
            Err(e) => return db_unavailable(e),
        }
    }
    let detail = json!({ "role": b.role, "before": current, "created": created });
    audit::record(&st, admin.entry("member.add", &org, &email, detail)).await;
    (StatusCode::CREATED, Json(out)).into_response()
}

/// Refuse to leave an org that has owners without any.
async fn last_owner_guard(
    pool: &sqlx::PgPool,
    org: &str,
    current: Role,
    next: Option<Role>,
) -> Result<(), Response> {
    if current != Role::Owner || next == Some(Role::Owner) {
        return Ok(());
    }
    match store::count_owners(pool, org).await {
        Ok(n) if n <= 1 => Err(err(
            StatusCode::CONFLICT,
            "this is the org's last owner; make someone else an owner first",
        )),
        Ok(_) => Ok(()),
        Err(e) => Err(db_unavailable(e)),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RoleBody {
    role: Role,
}

/// The member's email, current role in `org` and whether they are an instance admin.
async fn member(
    pool: &sqlx::PgPool,
    org: &str,
    user_id: i64,
) -> Result<(String, Role, bool), Response> {
    let user = store::user(pool, user_id).await.map_err(db_unavailable)?;
    let role = store::role_of(pool, user_id, org)
        .await
        .map_err(db_unavailable)?;
    match (user, role) {
        (Some((_, email, instance_admin, _)), Some(role)) => Ok((email, role, instance_admin)),
        _ => Err(err(StatusCode::NOT_FOUND, "not a member of this org")),
    }
}

async fn update_member(
    State(st): State<Arc<AppState>>,
    Path((org, user_id)): Path<(String, i64)>,
    admin: Admin,
    Json(b): Json<RoleBody>,
) -> Response {
    if let Err(r) = admin.org(&st, &org, Role::Admin) {
        return r;
    }
    let pool = match db(&st) {
        Ok(p) => p,
        Err(r) => return r,
    };
    let (email, current, _) = match member(pool, &org, user_id).await {
        Ok(m) => m,
        Err(r) => return r,
    };
    for role in [current, b.role] {
        if let Err(r) = may_manage(&admin, &org, role) {
            return r;
        }
    }
    if let Err(r) = last_owner_guard(pool, &org, current, Some(b.role)).await {
        return r;
    }
    if let Err(e) = store::set_role(pool, user_id, &org, b.role).await {
        return db_unavailable(e);
    }
    let detail = json!({ "changes": [{ "path": "role", "before": current, "after": b.role }] });
    audit::record(&st, admin.entry("member.role", &org, &email, detail)).await;
    Json(json!({ "user_id": user_id, "role": b.role })).into_response()
}

async fn remove_member(
    State(st): State<Arc<AppState>>,
    Path((org, user_id)): Path<(String, i64)>,
    admin: Admin,
) -> Response {
    if let Err(r) = admin.org(&st, &org, Role::Admin) {
        return r;
    }
    let pool = match db(&st) {
        Ok(p) => p,
        Err(r) => return r,
    };
    let (email, current, _) = match member(pool, &org, user_id).await {
        Ok(m) => m,
        Err(r) => return r,
    };
    if let Err(r) = may_manage(&admin, &org, current) {
        return r;
    }
    if let Err(r) = last_owner_guard(pool, &org, current, None).await {
        return r;
    }
    if let Err(e) = store::remove_member(pool, user_id, &org).await {
        return db_unavailable(e);
    }
    let detail = json!({ "role": current });
    audit::record(&st, admin.entry("member.remove", &org, &email, detail)).await;
    StatusCode::NO_CONTENT.into_response()
}

/// A new one-time link for a member to set (or reset) their password.
async fn member_setup_link(
    State(st): State<Arc<AppState>>,
    Path((org, user_id)): Path<(String, i64)>,
    admin: Admin,
) -> Response {
    if let Err(r) = admin.org(&st, &org, Role::Admin) {
        return r;
    }
    let pool = match db(&st) {
        Ok(p) => p,
        Err(r) => return r,
    };
    if !st.rt().cfg.auth.password_login {
        return err(
            StatusCode::CONFLICT,
            "password sign-in is turned off; members use single sign-on",
        );
    }
    let (email, current, instance_admin) = match member(pool, &org, user_id).await {
        Ok(m) => m,
        Err(r) => return r,
    };
    // A reset link is a way into the account: only for people the caller could manage anyway.
    if instance_admin && !admin.instance_admin {
        return err(
            StatusCode::FORBIDDEN,
            "only instance admins can reset an instance admin",
        );
    }
    if let Err(r) = may_manage(&admin, &org, current) {
        return r;
    }
    match store::create_setup_token(pool, user_id, SETUP_LINK_HOURS).await {
        Ok(token) => {
            let entry = admin.entry("member.setup_link", &org, &email, json!({}));
            audit::record(&st, entry).await;
            Json(setup_link(&st, &token)).into_response()
        }
        Err(e) => db_unavailable(e),
    }
}

// --- Everyone on the install (instance admins) ---

async fn list_users(State(st): State<Arc<AppState>>, admin: Admin) -> Response {
    if let Err(r) = admin.instance() {
        return r;
    }
    let pool = match db(&st) {
        Ok(p) => p,
        Err(r) => return r,
    };
    match store::list_users(pool).await {
        Ok(users) => Json(json!({ "users": users })).into_response(),
        Err(e) => db_unavailable(e),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NewUserBody {
    email: String,
    name: Option<String>,
    #[serde(default)]
    instance_admin: bool,
}

async fn create_user(
    State(st): State<Arc<AppState>>,
    admin: Admin,
    Json(b): Json<NewUserBody>,
) -> Response {
    if let Err(r) = admin.instance() {
        return r;
    }
    let pool = match db(&st) {
        Ok(p) => p,
        Err(r) => return r,
    };
    let Some(email) = normalize_email(&b.email) else {
        return err(StatusCode::BAD_REQUEST, "enter a valid email address");
    };
    let name: String = b
        .name
        .as_deref()
        .unwrap_or("")
        .trim()
        .chars()
        .take(200)
        .collect();
    let result = async {
        let (id, created) = store::upsert_user(pool, &email, &name).await?;
        if b.instance_admin {
            store::set_instance_admin(pool, id, true).await?;
        }
        let token = store::create_setup_token(pool, id, SETUP_LINK_HOURS).await?;
        anyhow::Ok((id, created, token))
    }
    .await;
    match result {
        Ok((id, created, token)) => {
            let detail = json!({ "instance_admin": b.instance_admin, "created": created });
            audit::record(&st, admin.entry_global("user.create", &email, detail)).await;
            let mut out = setup_link(&st, &token);
            out["id"] = json!(id);
            out["email"] = json!(email);
            (StatusCode::CREATED, Json(out)).into_response()
        }
        Err(e) => db_unavailable(e),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UserUpdate {
    instance_admin: Option<bool>,
    disabled: Option<bool>,
}

async fn update_user(
    State(st): State<Arc<AppState>>,
    Path(user_id): Path<i64>,
    admin: Admin,
    Json(b): Json<UserUpdate>,
) -> Response {
    if let Err(r) = admin.instance() {
        return r;
    }
    let pool = match db(&st) {
        Ok(p) => p,
        Err(r) => return r,
    };
    let user = match store::user(pool, user_id).await {
        Ok(u) => u,
        Err(e) => return db_unavailable(e),
    };
    let Some((_, email, was_admin, was_disabled)) = user else {
        return err(StatusCode::NOT_FOUND, "unknown user");
    };
    let losing_admin =
        was_admin && !was_disabled && (b.instance_admin == Some(false) || b.disabled == Some(true));
    if losing_admin {
        match store::count_instance_admins(pool).await {
            Ok(n) if n <= 1 => {
                return err(
                    StatusCode::CONFLICT,
                    "this is the last instance admin; make someone else one first",
                );
            }
            Ok(_) => {}
            Err(e) => return db_unavailable(e),
        }
    }
    if b.disabled == Some(true) && admin.user_id == Some(user_id) {
        return err(StatusCode::CONFLICT, "you cannot disable your own account");
    }
    let result = async {
        if let Some(on) = b.instance_admin {
            store::set_instance_admin(pool, user_id, on).await?;
        }
        if let Some(on) = b.disabled {
            store::set_disabled(pool, user_id, on).await?;
        }
        anyhow::Ok(())
    }
    .await;
    if let Err(e) = result {
        return db_unavailable(e);
    }
    let mut changes = Vec::new();
    if let Some(on) = b.instance_admin.filter(|on| *on != was_admin) {
        changes.push(json!({ "path": "instance_admin", "before": was_admin, "after": on }));
    }
    if let Some(on) = b.disabled.filter(|on| *on != was_disabled) {
        changes.push(json!({ "path": "disabled", "before": was_disabled, "after": on }));
    }
    if !changes.is_empty() {
        let entry = admin.entry_global("user.update", &email, json!({ "changes": changes }));
        audit::record(&st, entry).await;
    }
    StatusCode::NO_CONTENT.into_response()
}
