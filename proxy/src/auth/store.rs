//! Users, org memberships, sessions, API tokens and single sign-on state, in Postgres.
//! Secrets (session cookies, tokens, setup links) are stored only as SHA-256 hashes.

use std::collections::HashMap;

use anyhow::Result;
use chrono::{DateTime, SecondsFormat, Utc};
use serde_json::{Value, json};
use sqlx::{PgPool, Row};

use super::{Role, random_token, sha256_hex};

/// Failed passwords in a row before an account is locked for a while.
pub const MAX_FAILED_LOGINS: i32 = 10;
pub const LOCK_MINUTES: i32 = 15;

/// A signed-in user and their roles.
#[derive(Debug, Clone)]
pub struct AuthUser {
    pub id: i64,
    pub email: String,
    pub instance_admin: bool,
    pub roles: HashMap<String, Role>,
}

pub struct Session {
    pub user: AuthUser,
    pub csrf: String,
}

fn ts(t: Option<DateTime<Utc>>) -> Value {
    t.map_or(Value::Null, |t| {
        json!(t.to_rfc3339_opts(SecondsFormat::Secs, true))
    })
}

pub async fn roles(pool: &PgPool, user_id: i64) -> Result<HashMap<String, Role>> {
    let rows: Vec<(String, String)> =
        sqlx::query_as("SELECT org_id, role FROM memberships WHERE user_id = $1")
            .bind(user_id)
            .fetch_all(pool)
            .await?;
    Ok(rows
        .into_iter()
        .filter_map(|(org, role)| Role::parse(&role).map(|r| (org, r)))
        .collect())
}

async fn auth_user(
    pool: &PgPool,
    id: i64,
    email: String,
    instance_admin: bool,
) -> Result<AuthUser> {
    Ok(AuthUser {
        id,
        email,
        instance_admin,
        roles: roles(pool, id).await?,
    })
}

// --- Sessions ---

/// The user behind a session cookie, if the session is still valid. Refreshes its idle timer.
pub async fn session_user(
    pool: &PgPool,
    id_hash: &str,
    idle_minutes: u32,
) -> Result<Option<Session>> {
    let row: Option<(i64, String, bool, String, DateTime<Utc>)> = sqlx::query_as(
        "SELECT u.id, u.email, u.instance_admin, s.csrf, s.last_seen_at \
         FROM sessions s JOIN users u ON u.id = s.user_id \
         WHERE s.id_hash = $1 AND s.expires_at > now() AND NOT u.disabled \
           AND s.last_seen_at > now() - make_interval(mins => $2)",
    )
    .bind(id_hash)
    .bind(idle_minutes as i32)
    .fetch_optional(pool)
    .await?;
    let Some((id, email, instance_admin, csrf, last_seen)) = row else {
        return Ok(None);
    };
    if Utc::now() - last_seen > chrono::Duration::seconds(60) {
        sqlx::query("UPDATE sessions SET last_seen_at = now() WHERE id_hash = $1")
            .bind(id_hash)
            .execute(pool)
            .await?;
    }
    Ok(Some(Session {
        user: auth_user(pool, id, email, instance_admin).await?,
        csrf,
    }))
}

/// Start a session; returns the cookie value and the CSRF token.
pub async fn create_session(
    pool: &PgPool,
    user_id: i64,
    via: &str,
    ip: Option<&str>,
    hours: u32,
) -> Result<(String, String)> {
    let token = random_token("");
    let csrf = random_token("");
    sqlx::query(
        "INSERT INTO sessions (id_hash, user_id, csrf, via, ip, expires_at) \
         VALUES ($1, $2, $3, $4, $5, now() + make_interval(hours => $6))",
    )
    .bind(sha256_hex(&token))
    .bind(user_id)
    .bind(&csrf)
    .bind(via)
    .bind(ip)
    .bind(hours as i32)
    .execute(pool)
    .await?;
    // Keep the table small: drop sessions that ended a day ago or more.
    sqlx::query("DELETE FROM sessions WHERE expires_at < now() - interval '1 day'")
        .execute(pool)
        .await?;
    Ok((token, csrf))
}

pub async fn delete_session(pool: &PgPool, id_hash: &str) -> Result<()> {
    sqlx::query("DELETE FROM sessions WHERE id_hash = $1")
        .bind(id_hash)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn delete_user_sessions(pool: &PgPool, user_id: i64) -> Result<()> {
    sqlx::query("DELETE FROM sessions WHERE user_id = $1")
        .bind(user_id)
        .execute(pool)
        .await?;
    Ok(())
}

/// End a user's sessions except `keep` (a session hash).
pub async fn delete_other_sessions(pool: &PgPool, user_id: i64, keep: &str) -> Result<()> {
    sqlx::query("DELETE FROM sessions WHERE user_id = $1 AND id_hash <> $2")
        .bind(user_id)
        .bind(keep)
        .execute(pool)
        .await?;
    Ok(())
}

// --- Password sign-in ---

pub struct LoginUser {
    pub id: i64,
    pub email: String,
    pub password_hash: Option<String>,
    pub disabled: bool,
    pub locked: bool,
}

pub async fn user_for_login(pool: &PgPool, email: &str) -> Result<Option<LoginUser>> {
    let row = sqlx::query(
        "SELECT id, email, password_hash, disabled, \
                coalesce(locked_until > now(), false) AS locked \
         FROM users WHERE email = $1",
    )
    .bind(email)
    .fetch_optional(pool)
    .await?;
    row.map(|r| {
        Ok(LoginUser {
            id: r.try_get("id")?,
            email: r.try_get("email")?,
            password_hash: r.try_get("password_hash")?,
            disabled: r.try_get("disabled")?,
            locked: r.try_get("locked")?,
        })
    })
    .transpose()
}

/// Count a failed password; returns true when this failure locked the account.
pub async fn login_failed(pool: &PgPool, user_id: i64) -> Result<bool> {
    let locked: bool = sqlx::query_scalar(
        "UPDATE users SET failed_logins = failed_logins + 1, \
           locked_until = CASE WHEN failed_logins + 1 >= $2 \
                               THEN now() + make_interval(mins => $3) ELSE locked_until END \
         WHERE id = $1 RETURNING failed_logins >= $2",
    )
    .bind(user_id)
    .bind(MAX_FAILED_LOGINS)
    .bind(LOCK_MINUTES)
    .fetch_one(pool)
    .await?;
    if locked {
        sqlx::query("UPDATE users SET failed_logins = 0 WHERE id = $1")
            .bind(user_id)
            .execute(pool)
            .await?;
    }
    Ok(locked)
}

pub async fn login_succeeded(pool: &PgPool, user_id: i64) -> Result<()> {
    sqlx::query(
        "UPDATE users SET failed_logins = 0, locked_until = NULL, last_login_at = now() \
         WHERE id = $1",
    )
    .bind(user_id)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn set_password(pool: &PgPool, user_id: i64, hash: &str) -> Result<()> {
    sqlx::query(
        "UPDATE users SET password_hash = $2, failed_logins = 0, locked_until = NULL WHERE id = $1",
    )
    .bind(user_id)
    .bind(hash)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn password_hash(pool: &PgPool, user_id: i64) -> Result<Option<String>> {
    Ok(
        sqlx::query_scalar("SELECT password_hash FROM users WHERE id = $1")
            .bind(user_id)
            .fetch_one(pool)
            .await?,
    )
}

// --- Setup links (set or reset a password) ---

pub async fn create_setup_token(pool: &PgPool, user_id: i64, hours: u32) -> Result<String> {
    let token = random_token("trs_");
    sqlx::query(
        "INSERT INTO setup_tokens (token_hash, user_id, expires_at) \
         VALUES ($1, $2, now() + make_interval(hours => $3))",
    )
    .bind(sha256_hex(&token))
    .bind(user_id)
    .bind(hours as i32)
    .execute(pool)
    .await?;
    Ok(token)
}

/// The user (id, email) a setup link is for, without using it up.
pub async fn peek_setup_token(pool: &PgPool, token: &str) -> Result<Option<(i64, String)>> {
    Ok(sqlx::query_as(
        "SELECT u.id, u.email FROM setup_tokens t JOIN users u ON u.id = t.user_id \
         WHERE t.token_hash = $1 AND t.used_at IS NULL AND t.expires_at > now() \
           AND NOT u.disabled",
    )
    .bind(sha256_hex(token))
    .fetch_optional(pool)
    .await?)
}

/// Use a setup link once; returns its user (id, email) if it is valid.
pub async fn take_setup_token(pool: &PgPool, token: &str) -> Result<Option<(i64, String)>> {
    Ok(sqlx::query_as(
        "UPDATE setup_tokens t SET used_at = now() FROM users u \
         WHERE t.token_hash = $1 AND t.used_at IS NULL AND t.expires_at > now() \
           AND u.id = t.user_id AND NOT u.disabled \
         RETURNING u.id, u.email",
    )
    .bind(sha256_hex(token))
    .fetch_optional(pool)
    .await?)
}

// --- Users and memberships ---

pub async fn count_users(pool: &PgPool) -> Result<i64> {
    Ok(sqlx::query_scalar("SELECT count(*) FROM users")
        .fetch_one(pool)
        .await?)
}

/// Find or create a user by email. Returns (id, created).
pub async fn upsert_user(pool: &PgPool, email: &str, name: &str) -> Result<(i64, bool)> {
    let created: Option<i64> = sqlx::query_scalar(
        "INSERT INTO users (email, name) VALUES ($1, $2) ON CONFLICT (email) DO NOTHING RETURNING id",
    )
    .bind(email)
    .bind(name)
    .fetch_optional(pool)
    .await?;
    if let Some(id) = created {
        return Ok((id, true));
    }
    let id = sqlx::query_scalar("SELECT id FROM users WHERE email = $1")
        .bind(email)
        .fetch_one(pool)
        .await?;
    Ok((id, false))
}

/// (id, email, instance_admin, disabled) of a user, by id.
pub async fn user(pool: &PgPool, id: i64) -> Result<Option<(i64, String, bool, bool)>> {
    Ok(
        sqlx::query_as("SELECT id, email, instance_admin, disabled FROM users WHERE id = $1")
            .bind(id)
            .fetch_optional(pool)
            .await?,
    )
}

pub async fn user_by_email(pool: &PgPool, email: &str) -> Result<Option<(i64, bool)>> {
    Ok(
        sqlx::query_as("SELECT id, disabled FROM users WHERE email = $1")
            .bind(email)
            .fetch_optional(pool)
            .await?,
    )
}

pub async fn set_name_if_empty(pool: &PgPool, id: i64, name: &str) -> Result<()> {
    sqlx::query("UPDATE users SET name = $2 WHERE id = $1 AND name = ''")
        .bind(id)
        .bind(name)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn set_instance_admin(pool: &PgPool, id: i64, on: bool) -> Result<()> {
    sqlx::query("UPDATE users SET instance_admin = $2 WHERE id = $1")
        .bind(id)
        .bind(on)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn set_disabled(pool: &PgPool, id: i64, on: bool) -> Result<()> {
    sqlx::query("UPDATE users SET disabled = $2 WHERE id = $1")
        .bind(id)
        .bind(on)
        .execute(pool)
        .await?;
    if on {
        delete_user_sessions(pool, id).await?;
        sqlx::query(
            "UPDATE api_tokens SET revoked_at = now() WHERE user_id = $1 AND revoked_at IS NULL",
        )
        .bind(id)
        .execute(pool)
        .await?;
    }
    Ok(())
}

pub async fn count_instance_admins(pool: &PgPool) -> Result<i64> {
    Ok(
        sqlx::query_scalar("SELECT count(*) FROM users WHERE instance_admin AND NOT disabled")
            .fetch_one(pool)
            .await?,
    )
}

fn user_json(r: &sqlx::postgres::PgRow) -> Result<Value> {
    Ok(json!({
        "id": r.try_get::<i64, _>("id")?,
        "email": r.try_get::<String, _>("email")?,
        "name": r.try_get::<String, _>("name")?,
        "has_password": r.try_get::<bool, _>("has_password")?,
        "instance_admin": r.try_get::<bool, _>("instance_admin")?,
        "disabled": r.try_get::<bool, _>("disabled")?,
        "last_login_at": ts(r.try_get("last_login_at")?),
    }))
}

/// Everyone, with their org roles.
pub async fn list_users(pool: &PgPool) -> Result<Vec<Value>> {
    let rows = sqlx::query(
        "SELECT u.id, u.email, u.name, u.password_hash IS NOT NULL AS has_password, \
                u.instance_admin, u.disabled, u.last_login_at, \
                coalesce(json_object_agg(m.org_id, m.role) FILTER (WHERE m.org_id IS NOT NULL), \
                         '{}')::jsonb AS roles \
         FROM users u LEFT JOIN memberships m ON m.user_id = u.id \
         GROUP BY u.id ORDER BY u.email",
    )
    .fetch_all(pool)
    .await?;
    rows.iter()
        .map(|r| {
            let mut v = user_json(r)?;
            v["roles"] = r.try_get::<Value, _>("roles")?;
            Ok(v)
        })
        .collect()
}

/// An org's members.
pub async fn members(pool: &PgPool, org: &str) -> Result<Vec<Value>> {
    let rows = sqlx::query(
        "SELECT u.id, u.email, u.name, u.password_hash IS NOT NULL AS has_password, \
                u.instance_admin, u.disabled, u.last_login_at, m.role \
         FROM memberships m JOIN users u ON u.id = m.user_id \
         WHERE m.org_id = $1 ORDER BY u.email",
    )
    .bind(org)
    .fetch_all(pool)
    .await?;
    rows.iter()
        .map(|r| {
            let mut v = user_json(r)?;
            v["role"] = json!(r.try_get::<String, _>("role")?);
            Ok(v)
        })
        .collect()
}

pub async fn role_of(pool: &PgPool, user_id: i64, org: &str) -> Result<Option<Role>> {
    let role: Option<String> =
        sqlx::query_scalar("SELECT role FROM memberships WHERE user_id = $1 AND org_id = $2")
            .bind(user_id)
            .bind(org)
            .fetch_optional(pool)
            .await?;
    Ok(role.as_deref().and_then(Role::parse))
}

pub async fn set_role(pool: &PgPool, user_id: i64, org: &str, role: Role) -> Result<()> {
    sqlx::query(
        "INSERT INTO memberships (user_id, org_id, role) VALUES ($1, $2, $3) \
         ON CONFLICT (user_id, org_id) DO UPDATE SET role = EXCLUDED.role",
    )
    .bind(user_id)
    .bind(org)
    .bind(role.as_str())
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn remove_member(pool: &PgPool, user_id: i64, org: &str) -> Result<bool> {
    let n = sqlx::query("DELETE FROM memberships WHERE user_id = $1 AND org_id = $2")
        .bind(user_id)
        .bind(org)
        .execute(pool)
        .await?
        .rows_affected();
    Ok(n > 0)
}

pub async fn count_owners(pool: &PgPool, org: &str) -> Result<i64> {
    Ok(sqlx::query_scalar(
        "SELECT count(*) FROM memberships m JOIN users u ON u.id = m.user_id \
         WHERE m.org_id = $1 AND m.role = 'owner' AND NOT u.disabled",
    )
    .bind(org)
    .fetch_one(pool)
    .await?)
}

pub async fn delete_org_members(pool: &PgPool, org: &str) -> Result<()> {
    sqlx::query("DELETE FROM memberships WHERE org_id = $1")
        .bind(org)
        .execute(pool)
        .await?;
    Ok(())
}

// --- API tokens ---

/// (token id, user id, email, instance admin, last used).
type TokenRow = (i64, i64, String, bool, Option<DateTime<Utc>>);

/// The user behind an API token, if it is valid. Notes when it was last used.
pub async fn api_token_user(pool: &PgPool, token_hash: &str) -> Result<Option<AuthUser>> {
    let row: Option<TokenRow> = sqlx::query_as(
        "SELECT t.id, u.id, u.email, u.instance_admin, t.last_used_at \
         FROM api_tokens t JOIN users u ON u.id = t.user_id \
         WHERE t.token_hash = $1 AND t.revoked_at IS NULL \
           AND (t.expires_at IS NULL OR t.expires_at > now()) AND NOT u.disabled",
    )
    .bind(token_hash)
    .fetch_optional(pool)
    .await?;
    let Some((token_id, id, email, instance_admin, last_used)) = row else {
        return Ok(None);
    };
    if last_used.is_none_or(|t| Utc::now() - t > chrono::Duration::seconds(60)) {
        sqlx::query("UPDATE api_tokens SET last_used_at = now() WHERE id = $1")
            .bind(token_id)
            .execute(pool)
            .await?;
    }
    Ok(Some(auth_user(pool, id, email, instance_admin).await?))
}

/// Returns (id, token, prefix); the token itself is shown once.
pub async fn create_api_token(
    pool: &PgPool,
    user_id: i64,
    name: &str,
    expires_days: Option<u32>,
) -> Result<(i64, String, String)> {
    let token = random_token(super::API_TOKEN_PREFIX);
    let prefix: String = token.chars().take(12).collect();
    let id = sqlx::query_scalar(
        "INSERT INTO api_tokens (user_id, name, token_hash, prefix, expires_at) \
         VALUES ($1, $2, $3, $4, now() + make_interval(days => $5)) RETURNING id",
    )
    .bind(user_id)
    .bind(name)
    .bind(sha256_hex(&token))
    .bind(&prefix)
    .bind(expires_days.map(|d| d as i32))
    .fetch_one(pool)
    .await?;
    Ok((id, token, prefix))
}

pub async fn list_api_tokens(pool: &PgPool, user_id: i64) -> Result<Vec<Value>> {
    let rows = sqlx::query(
        "SELECT id, name, prefix, created_at, last_used_at, expires_at FROM api_tokens \
         WHERE user_id = $1 AND revoked_at IS NULL ORDER BY id DESC",
    )
    .bind(user_id)
    .fetch_all(pool)
    .await?;
    rows.iter()
        .map(|r| {
            Ok(json!({
                "id": r.try_get::<i64, _>("id")?,
                "name": r.try_get::<String, _>("name")?,
                "prefix": r.try_get::<String, _>("prefix")?,
                "created_at": ts(r.try_get("created_at")?),
                "last_used_at": ts(r.try_get("last_used_at")?),
                "expires_at": ts(r.try_get("expires_at")?),
            }))
        })
        .collect()
}

pub async fn revoke_api_token(pool: &PgPool, user_id: i64, id: i64) -> Result<bool> {
    let n = sqlx::query(
        "UPDATE api_tokens SET revoked_at = now() \
         WHERE id = $1 AND user_id = $2 AND revoked_at IS NULL",
    )
    .bind(id)
    .bind(user_id)
    .execute(pool)
    .await?
    .rows_affected();
    Ok(n > 0)
}

// --- Single sign-on ---

pub async fn sso_config(pool: &PgPool) -> Result<Option<Value>> {
    Ok(
        sqlx::query_scalar("SELECT config FROM sso_config WHERE id = 1")
            .fetch_optional(pool)
            .await?,
    )
}

pub async fn put_sso_config(pool: &PgPool, config: &Value) -> Result<()> {
    sqlx::query(
        "INSERT INTO sso_config (id, config) VALUES (1, $1) \
         ON CONFLICT (id) DO UPDATE SET config = EXCLUDED.config, updated_at = now()",
    )
    .bind(config)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn start_sso_login(
    pool: &PgPool,
    state: &str,
    nonce: &str,
    verifier: &str,
) -> Result<()> {
    sqlx::query("DELETE FROM sso_logins WHERE created_at < now() - interval '10 minutes'")
        .execute(pool)
        .await?;
    sqlx::query("INSERT INTO sso_logins (state_hash, nonce, verifier) VALUES ($1, $2, $3)")
        .bind(sha256_hex(state))
        .bind(nonce)
        .bind(verifier)
        .execute(pool)
        .await?;
    Ok(())
}

/// Use a sign-in's state once; returns (nonce, PKCE verifier) if it is recent.
pub async fn take_sso_login(pool: &PgPool, state: &str) -> Result<Option<(String, String)>> {
    Ok(sqlx::query_as(
        "DELETE FROM sso_logins WHERE state_hash = $1 \
           AND created_at > now() - interval '10 minutes' RETURNING nonce, verifier",
    )
    .bind(sha256_hex(state))
    .fetch_optional(pool)
    .await?)
}
