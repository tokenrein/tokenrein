//! Single sign-on with OpenID Connect (Okta, Microsoft Entra ID, Google Workspace, Auth0,
//! Keycloak, ...): authorization code flow with PKCE, ID tokens checked against the provider's
//! published keys.
//!
//! The settings live in Postgres (`sso_config`) and are edited from the dashboard; the client
//! secret is encrypted with the proxy's secrets key, or read from an environment variable.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use jsonwebtoken::jwk::JwkSet;
use jsonwebtoken::{Algorithm, DecodingKey, Validation};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use super::{Role, random_token, store};
use crate::proxy::AppState;

/// How long the provider's metadata and keys are trusted before they are fetched again.
const METADATA_TTL: Duration = Duration::from_secs(3600);
const SECRET_CONTEXT: &str = "sso:client_secret";

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct SsoConfig {
    pub enabled: bool,
    /// Shown on the sign-in button ("Sign in with Okta").
    pub label: String,
    pub issuer: String,
    pub client_id: String,
    /// Encrypted with the proxy's secrets key.
    pub client_secret: Option<SealedSecret>,
    /// Or: the env var holding the client secret.
    pub client_secret_env: Option<String>,
    /// Send the client secret as HTTP Basic auth (default) or in the form body ("post").
    pub token_auth: TokenAuth,
    /// Only these email domains may sign in (empty: any the provider vouches for).
    pub allowed_domains: Vec<String>,
    /// Create an account on first sign-in; otherwise an admin adds people first.
    pub auto_create: bool,
    /// Org (and role) given to accounts created on first sign-in, when no group mapping applies.
    pub default_org: Option<String>,
    pub default_role: Option<Role>,
    /// The ID token claim listing the person's groups.
    pub groups_claim: String,
    /// Roles from groups, kept in sync at every sign-in for the orgs listed here.
    pub group_roles: Vec<GroupRole>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SealedSecret {
    pub sealed: String,
    pub last4: String,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TokenAuth {
    #[default]
    Basic,
    Post,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupRole {
    pub group: String,
    pub org: String,
    pub role: Role,
}

impl SsoConfig {
    pub async fn load(st: &AppState) -> Result<Option<Self>> {
        let Some(db) = &st.db else { return Ok(None) };
        match store::sso_config(db.pool()).await? {
            Some(v) => Ok(Some(serde_json::from_value(v)?)),
            None => Ok(None),
        }
    }

    /// The settings as shown to admins: everything but the secret.
    pub fn public(&self) -> Value {
        let mut v = serde_json::to_value(self).unwrap_or_default();
        v["client_secret"] = match &self.client_secret {
            Some(s) => serde_json::json!({ "last4": s.last4 }),
            None => Value::Null,
        };
        v
    }

    pub fn groups_claim(&self) -> &str {
        if self.groups_claim.is_empty() {
            "groups"
        } else {
            &self.groups_claim
        }
    }

    fn client_secret(&self, st: &AppState) -> Result<Option<String>> {
        let rt = st.rt();
        if let Some(name) = &self.client_secret_env {
            return Ok(rt.env_var(name));
        }
        match (&self.client_secret, rt.secrets()) {
            // During a secrets key rotation the secret may still be sealed with the old key.
            (Some(s), Some(b)) => match b.open(SECRET_CONTEXT, &s.sealed) {
                Ok(plain) => Ok(Some(plain)),
                Err(e) => match rt.previous_secrets() {
                    Some(prev) => Ok(Some(prev.open(SECRET_CONTEXT, &s.sealed)?)),
                    None => Err(e),
                },
            },
            (Some(_), None) => {
                bail!("the SSO client secret is saved but the secrets key is not set")
            }
            (None, _) => Ok(None),
        }
    }

    /// Re-encrypt the saved client secret with the current secrets key if it is still sealed
    /// with the previous one. Returns whether anything changed.
    pub async fn rotate_secret(st: &AppState) -> Result<bool> {
        let (Some(db), Some(mut cfg)) = (&st.db, Self::load(st).await?) else {
            return Ok(false);
        };
        let rt = st.rt();
        let (Some(saved), Some(current)) = (&cfg.client_secret, rt.secrets()) else {
            return Ok(false);
        };
        if current.open(SECRET_CONTEXT, &saved.sealed).is_ok() {
            return Ok(false);
        }
        let plain = rt
            .previous_secrets()
            .and_then(|p| p.open(SECRET_CONTEXT, &saved.sealed).ok())
            .ok_or_else(|| {
                anyhow!(
                    "neither the secrets key nor the previous one can decrypt the SSO client secret"
                )
            })?;
        let last4 = saved.last4.clone();
        cfg.client_secret = Some(SealedSecret {
            sealed: current.seal(SECRET_CONTEXT, &plain)?,
            last4,
        });
        store::put_sso_config(db.pool(), &serde_json::to_value(&cfg)?).await?;
        Ok(true)
    }

    pub fn seal_secret(st: &AppState, secret: &str) -> Result<SealedSecret> {
        let rt = st.rt();
        let b = rt
            .secrets()
            .ok_or_else(|| anyhow!("set the secrets key to save a client secret here"))?;
        let chars: Vec<char> = secret.chars().collect();
        Ok(SealedSecret {
            sealed: b.seal(SECRET_CONTEXT, secret)?,
            last4: chars[chars.len().saturating_sub(4)..].iter().collect(),
        })
    }
}

/// The identity provider's endpoints and signing keys.
#[derive(Clone)]
pub struct Provider {
    pub issuer: String,
    pub authorization_endpoint: String,
    pub token_endpoint: String,
    jwks: JwkSet,
    fetched: Instant,
    /// The configured issuer this was fetched for.
    for_issuer: String,
}

#[derive(Deserialize)]
struct Discovery {
    issuer: String,
    authorization_endpoint: String,
    token_endpoint: String,
    jwks_uri: String,
}

/// Fetch (or reuse) the provider's metadata and keys.
pub async fn provider(st: &AppState, cfg: &SsoConfig, refresh: bool) -> Result<Provider> {
    let mut cache = st.auth.sso.lock().await;
    if let Some(p) = cache.as_ref()
        && !refresh
        && p.for_issuer == cfg.issuer
        && p.fetched.elapsed() < METADATA_TTL
    {
        return Ok(p.clone());
    }
    let base = cfg.issuer.trim_end_matches('/');
    let d: Discovery = get_json(st, &format!("{base}/.well-known/openid-configuration"))
        .await
        .context("reading the provider's OpenID configuration")?;
    // The document must describe the issuer it was fetched for (OIDC Discovery 4.3).
    if d.issuer.trim_end_matches('/') != base {
        bail!(
            "the provider says its issuer is {:?}, not {:?}",
            d.issuer,
            cfg.issuer
        );
    }
    let jwks: JwkSet = get_json(st, &d.jwks_uri)
        .await
        .context("reading the provider's signing keys")?;
    let p = Provider {
        issuer: d.issuer,
        authorization_endpoint: d.authorization_endpoint,
        token_endpoint: d.token_endpoint,
        jwks,
        fetched: Instant::now(),
        for_issuer: cfg.issuer.clone(),
    };
    *cache = Some(p.clone());
    Ok(p)
}

async fn get_json<T: serde::de::DeserializeOwned>(st: &AppState, url: &str) -> Result<T> {
    let resp = st
        .http
        .get(url)
        .timeout(Duration::from_secs(10))
        .send()
        .await?
        .error_for_status()?;
    Ok(resp.json().await?)
}

pub fn redirect_uri(st: &AppState) -> Option<String> {
    let rt = st.rt();
    rt.cfg
        .auth
        .public_url
        .as_deref()
        .map(|u| format!("{}/auth/sso/callback", u.trim_end_matches('/')))
}

/// Where to send the browser to sign in.
pub async fn authorize_url(st: &AppState, cfg: &SsoConfig) -> Result<String> {
    let db = st.db.as_ref().ok_or_else(|| anyhow!("no database"))?;
    let redirect = redirect_uri(st).ok_or_else(|| anyhow!("auth.public_url is not set"))?;
    let p = provider(st, cfg, false).await?;
    let (state, nonce, verifier) = (random_token(""), random_token(""), random_token(""));
    store::start_sso_login(db.pool(), &state, &nonce, &verifier).await?;
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    let mut scope = "openid email profile".to_string();
    if !cfg.group_roles.is_empty() && cfg.groups_claim() == "groups" {
        // Some providers (Okta, Keycloak) only include groups when asked for them.
        scope.push_str(" groups");
    }
    let mut url = url::Url::parse(&p.authorization_endpoint)?;
    url.query_pairs_mut()
        .append_pair("response_type", "code")
        .append_pair("client_id", &cfg.client_id)
        .append_pair("redirect_uri", &redirect)
        .append_pair("scope", &scope)
        .append_pair("state", &state)
        .append_pair("nonce", &nonce)
        .append_pair("code_challenge", &challenge)
        .append_pair("code_challenge_method", "S256");
    Ok(url.into())
}

/// Who the provider says signed in.
#[derive(Debug, Clone)]
pub struct Identity {
    pub email: String,
    pub name: String,
    pub groups: Vec<String>,
}

#[derive(Deserialize)]
struct TokenResponse {
    id_token: String,
}

/// Finish a sign-in: check the state, trade the code for an ID token, and check the token.
/// Errors are meant for the person signing in.
pub async fn finish(st: &AppState, cfg: &SsoConfig, code: &str, state: &str) -> Result<Identity> {
    let db = st.db.as_ref().ok_or_else(|| anyhow!("no database"))?;
    let Some((nonce, verifier)) = store::take_sso_login(db.pool(), state).await? else {
        bail!("this sign-in link expired or was already used; start again");
    };
    let redirect = redirect_uri(st).ok_or_else(|| anyhow!("auth.public_url is not set"))?;
    let p = provider(st, cfg, false).await?;
    let secret = cfg.client_secret(st)?;
    let mut form = vec![
        ("grant_type", "authorization_code".to_string()),
        ("code", code.to_string()),
        ("redirect_uri", redirect),
        ("code_verifier", verifier),
    ];
    let mut req = st
        .http
        .post(&p.token_endpoint)
        .timeout(Duration::from_secs(10));
    match (&secret, cfg.token_auth) {
        (Some(s), TokenAuth::Basic) => req = req.basic_auth(&cfg.client_id, Some(s)),
        (Some(s), TokenAuth::Post) => {
            form.push(("client_id", cfg.client_id.clone()));
            form.push(("client_secret", s.clone()));
        }
        (None, _) => form.push(("client_id", cfg.client_id.clone())),
    }
    let resp = req
        .form(&form)
        .send()
        .await
        .context("calling the provider")?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body: String = resp
            .text()
            .await
            .unwrap_or_default()
            .chars()
            .take(300)
            .collect();
        tracing::warn!(%status, %body, "SSO token exchange failed");
        bail!("the identity provider refused the sign-in (HTTP {status})");
    }
    let tokens: TokenResponse = resp.json().await.context("reading the provider's answer")?;
    let claims = match check_id_token(&p, cfg, &tokens.id_token) {
        Err(e) if e.to_string().contains("unknown signing key") => {
            // Keys rotate; fetch them again once.
            let p = provider(st, cfg, true).await?;
            check_id_token(&p, cfg, &tokens.id_token)?
        }
        other => other?,
    };
    if claims.get("nonce").and_then(Value::as_str) != Some(nonce.as_str()) {
        bail!("the sign-in did not match this browser session; start again");
    }
    identity(cfg, &claims)
}

const ALGORITHMS: [Algorithm; 8] = [
    Algorithm::RS256,
    Algorithm::RS384,
    Algorithm::RS512,
    Algorithm::PS256,
    Algorithm::PS384,
    Algorithm::PS512,
    Algorithm::ES256,
    Algorithm::ES384,
];

fn check_id_token(
    p: &Provider,
    cfg: &SsoConfig,
    token: &str,
) -> Result<serde_json::Map<String, Value>> {
    let header = jsonwebtoken::decode_header(token).context("the ID token is malformed")?;
    if !ALGORITHMS.contains(&header.alg) {
        bail!("the ID token uses an algorithm Tokenrein does not accept");
    }
    let jwk = match &header.kid {
        Some(kid) => p.jwks.find(kid),
        None if p.jwks.keys.len() == 1 => p.jwks.keys.first(),
        None => None,
    }
    .ok_or_else(|| anyhow!("the ID token is signed with an unknown signing key"))?;
    let key = DecodingKey::from_jwk(jwk)?;
    let mut v = Validation::new(header.alg);
    v.set_audience(&[&cfg.client_id]);
    v.set_issuer(&[&p.issuer]);
    v.set_required_spec_claims(&["exp", "iss", "aud", "sub"]);
    v.leeway = 60;
    let data = jsonwebtoken::decode::<serde_json::Map<String, Value>>(token, &key, &v)
        .map_err(|e| anyhow!("the ID token is not valid: {e}"))?;
    Ok(data.claims)
}

fn identity(cfg: &SsoConfig, c: &serde_json::Map<String, Value>) -> Result<Identity> {
    let s = |k: &str| {
        c.get(k)
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or("")
    };
    // Entra ID often leaves out `email`; its `preferred_username` is the sign-in address.
    let email = [s("email"), s("preferred_username"), s("upn")]
        .into_iter()
        .find(|e| e.contains('@'))
        .unwrap_or("")
        .to_lowercase();
    if email.is_empty() {
        bail!("the identity provider did not share an email address");
    }
    let verified = match c.get("email_verified") {
        Some(Value::Bool(b)) => *b,
        Some(Value::String(s)) => s == "true",
        _ => true,
    };
    if !verified {
        bail!("your email address is not verified with the identity provider");
    }
    let domain = email.rsplit('@').next().unwrap_or("");
    if !cfg.allowed_domains.is_empty()
        && !cfg.allowed_domains.iter().any(|d| {
            d.trim()
                .trim_start_matches('@')
                .eq_ignore_ascii_case(domain)
        })
    {
        bail!("accounts from {domain} cannot sign in here");
    }
    let groups = match c.get(cfg.groups_claim()) {
        Some(Value::Array(a)) => a
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect(),
        Some(Value::String(g)) => vec![g.clone()],
        _ => Vec::new(),
    };
    Ok(Identity {
        email,
        name: s("name").chars().take(200).collect(),
        groups,
    })
}

/// The roles the person's groups give them, per org (highest wins).
pub fn group_roles(cfg: &SsoConfig, groups: &[String]) -> HashMap<String, Role> {
    let mut out: HashMap<String, Role> = HashMap::new();
    for m in &cfg.group_roles {
        if groups.iter().any(|g| g == &m.group) {
            let r = out.entry(m.org.clone()).or_insert(m.role);
            *r = (*r).max(m.role);
        }
    }
    out
}

/// Find or create the account for a signed-in identity and bring its roles in line with its
/// groups. Returns (user id, created); errors are meant for the person signing in.
pub async fn provision(st: &AppState, cfg: &SsoConfig, id: &Identity) -> Result<(i64, bool)> {
    let db = st.db.as_ref().ok_or_else(|| anyhow!("no database"))?;
    let pool = db.pool();
    let (user_id, created) = match store::user_by_email(pool, &id.email).await? {
        Some((_, true)) => bail!("your Tokenrein account is disabled"),
        Some((uid, false)) => (uid, false),
        None if cfg.auto_create => store::upsert_user(pool, &id.email, &id.name).await?,
        None => bail!(
            "{} has no Tokenrein account yet; ask an admin to add you",
            id.email
        ),
    };
    if !id.name.is_empty() {
        store::set_name_if_empty(pool, user_id, &id.name).await?;
    }
    let orgs: Vec<String> = st.rt().cfg.orgs.iter().map(|o| o.id.clone()).collect();
    let wanted = group_roles(cfg, &id.groups);
    let managed: HashSet<&str> = cfg.group_roles.iter().map(|m| m.org.as_str()).collect();
    for org in managed {
        match wanted.get(org) {
            Some(role) if orgs.iter().any(|o| o == org) => {
                store::set_role(pool, user_id, org, *role).await?
            }
            _ => {
                store::remove_member(pool, user_id, org).await?;
            }
        }
    }
    if created
        && wanted.is_empty()
        && let Some(org) = cfg
            .default_org
            .as_deref()
            .filter(|o| orgs.iter().any(|x| x == o))
    {
        let role = cfg.default_role.unwrap_or(Role::Viewer);
        store::set_role(pool, user_id, org, role).await?;
    }
    Ok((user_id, created))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn cfg() -> SsoConfig {
        SsoConfig {
            allowed_domains: vec!["example.com".into()],
            group_roles: vec![
                GroupRole {
                    group: "eng".into(),
                    org: "acme".into(),
                    role: Role::Viewer,
                },
                GroupRole {
                    group: "platform".into(),
                    org: "acme".into(),
                    role: Role::Admin,
                },
            ],
            ..Default::default()
        }
    }

    fn claims(v: Value) -> serde_json::Map<String, Value> {
        v.as_object().unwrap().clone()
    }

    #[test]
    fn identity_uses_entra_style_usernames_and_checks_domains() {
        let c = claims(json!({"preferred_username": "Ana@Example.com", "groups": ["eng"]}));
        let id = identity(&cfg(), &c).unwrap();
        assert_eq!(id.email, "ana@example.com");
        assert_eq!(id.groups, ["eng"]);
        assert!(identity(&cfg(), &claims(json!({"email": "x@other.org"}))).is_err());
        let unverified = claims(json!({"email": "a@example.com", "email_verified": false}));
        assert!(identity(&cfg(), &unverified).is_err());
        assert!(identity(&cfg(), &claims(json!({"sub": "123"}))).is_err());
    }

    #[test]
    fn the_highest_group_role_wins() {
        let r = group_roles(&cfg(), &["eng".into(), "platform".into()]);
        assert_eq!(r.get("acme"), Some(&Role::Admin));
        assert!(group_roles(&cfg(), &["sales".into()]).is_empty());
    }
}
