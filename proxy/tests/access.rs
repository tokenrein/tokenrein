//! People, roles, sessions, API tokens and single sign-on (against a mock identity provider).

mod common;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use axum::extract::{Form, State};
use axum::routing::{get, post};
use axum::{Json, Router};
use reqwest::{Method, StatusCode};
use serde_json::{Value, json};
use tokenlens_proxy::config::Resolved;
use tokenlens_proxy::{AppState, admin, build_state, control};

const ADMIN_TOKEN: &str = "admin-token-0123456789";
const SECRETS_KEY: &str = "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f";
const PASSWORD: &str = "a long enough passphrase";
const PUBLIC_URL: &str = "http://tokenlens.test";

fn config(redis: &str) -> String {
    format!(
        r#"
        [server]
        listen = "127.0.0.1:0"
        admin_token_env = "ADMIN"
        secrets_key_env = "SECRETS"
        [redis]
        url = "{redis}"
        [database]
        url_env = "DB"
        [auth]
        public_url = "{PUBLIC_URL}"
        [[orgs]]
        id = "acme"
        [[orgs.projects]]
        id = "bot"
        [[orgs]]
        id = "other"
        "#
    )
}

struct T {
    _redis: common::RedisServer,
    _pg: common::PostgresServer,
    url: String,
    http: reqwest::Client,
}

/// A signed-in browser: its session cookie and CSRF token.
#[derive(Clone)]
struct Sess {
    cookie: String,
    csrf: String,
}

impl T {
    async fn start() -> Option<Self> {
        let redis = common::RedisServer::start()?;
        let pg = common::PostgresServer::start()?;
        let db = pg.url.clone();
        let rt = Resolved::from_toml(&config(&redis.url), move |name| match name {
            "ADMIN" => Some(ADMIN_TOKEN.into()),
            "SECRETS" => Some(SECRETS_KEY.into()),
            "DB" => Some(db.clone()),
            _ => None,
        })
        .unwrap();
        let state = build_state(rt).await.unwrap();
        control::init(&state).await.unwrap();
        let url = serve(state).await;
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap();
        Some(Self {
            _redis: redis,
            _pg: pg,
            url,
            http,
        })
    }

    async fn send(&self, req: reqwest::RequestBuilder, body: Option<Value>) -> (StatusCode, Value) {
        let req = match body {
            Some(b) => req.json(&b),
            None => req,
        };
        let resp = req.send().await.unwrap();
        let status = resp.status();
        (status, resp.json().await.unwrap_or(Value::Null))
    }

    /// As the break-glass admin token.
    async fn root(&self, method: Method, path: &str, body: Option<Value>) -> (StatusCode, Value) {
        let req = self
            .http
            .request(method, format!("{}{path}", self.url))
            .bearer_auth(ADMIN_TOKEN);
        self.send(req, body).await
    }

    async fn as_user(
        &self,
        s: &Sess,
        method: Method,
        path: &str,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        let req = self
            .http
            .request(method, format!("{}{path}", self.url))
            .header("cookie", format!("tl_session={}", s.cookie))
            .header("x-csrf-token", &s.csrf);
        self.send(req, body).await
    }

    async fn get(&self, s: &Sess, path: &str) -> (StatusCode, Value) {
        self.as_user(s, Method::GET, path, None).await
    }

    /// Add `email` to `org` as `role` (with the admin token) and set their password.
    async fn member(&self, org: &str, email: &str, role: &str) -> (i64, Sess) {
        let body = json!({"email": email, "role": role});
        let (status, added) = self
            .root(
                Method::POST,
                &format!("/admin/orgs/{org}/members"),
                Some(body),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "{added}");
        let token = setup_token(&added);
        assert_eq!(added["setup_url"], format!("{PUBLIC_URL}/#setup={token}"));
        let sess = self.setup(&token, PASSWORD).await.unwrap();
        (added["user_id"].as_i64().unwrap(), sess)
    }

    async fn setup(&self, token: &str, password: &str) -> Result<Sess, (StatusCode, Value)> {
        let resp = self
            .http
            .post(format!("{}/auth/setup", self.url))
            .json(&json!({"token": token, "password": password}))
            .send()
            .await
            .unwrap();
        signed_in(resp).await
    }

    async fn login(&self, email: &str, password: &str) -> Result<Sess, (StatusCode, Value)> {
        let resp = self
            .http
            .post(format!("{}/auth/login", self.url))
            .json(&json!({"email": email, "password": password}))
            .send()
            .await
            .unwrap();
        signed_in(resp).await
    }
}

fn setup_token(added: &Value) -> String {
    added["setup_path"]
        .as_str()
        .unwrap()
        .trim_start_matches("/#setup=")
        .to_string()
}

async fn serve(state: Arc<AppState>) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(
            listener,
            admin::router(state).into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .await
        .unwrap()
    });
    format!("http://{addr}")
}

fn session_cookie(resp: &reqwest::Response) -> Option<String> {
    resp.headers()
        .get_all("set-cookie")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .find_map(|c| c.strip_prefix("tl_session="))
        .map(|c| c.split(';').next().unwrap().to_string())
        .filter(|c| !c.is_empty())
}

async fn signed_in(resp: reqwest::Response) -> Result<Sess, (StatusCode, Value)> {
    let status = resp.status();
    let cookie = session_cookie(&resp);
    let flags = resp
        .headers()
        .get("set-cookie")
        .map(|v| v.to_str().unwrap().to_string());
    let body: Value = resp.json().await.unwrap_or(Value::Null);
    if !status.is_success() {
        return Err((status, body));
    }
    let flags = flags.unwrap();
    assert!(
        flags.contains("HttpOnly") && flags.contains("SameSite=Lax"),
        "{flags}"
    );
    Ok(Sess {
        cookie: cookie.unwrap(),
        csrf: body["csrf"].as_str().unwrap().to_string(),
    })
}

fn org_ids(overview: &Value) -> Vec<&str> {
    overview["orgs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|o| o["id"].as_str().unwrap())
        .collect()
}

fn actions(log: &Value) -> Vec<&str> {
    log["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["action"].as_str().unwrap())
        .collect()
}

#[tokio::test]
async fn roles_decide_what_people_can_see_and_do() {
    let Some(t) = T::start().await else { return };
    let (_, cfg) = t.root(Method::GET, "/auth/config", None).await;
    assert_eq!(cfg["password_login"], true);
    assert_eq!(cfg["users"], 0);

    let (vid, vera) = t.member("acme", "Vera@Example.com", "viewer").await;
    // Viewers see their own org only.
    let (status, ov) = t.as_user(&vera, Method::GET, "/admin/overview", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(org_ids(&ov), ["acme"]);
    let (_, me) = t.as_user(&vera, Method::GET, "/auth/me", None).await;
    assert_eq!(me["email"], "vera@example.com");
    assert_eq!(me["roles"], json!({"acme": "viewer"}));
    assert_eq!(
        t.get(&vera, "/admin/orgs/other/runs").await.0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        t.get(&vera, "/admin/orgs/acme/runs").await.0,
        StatusCode::OK
    );
    assert_eq!(t.get(&vera, "/admin/users").await.0, StatusCode::FORBIDDEN);

    let budgets = json!({"budgets": {"run": {"limit_usd": 1.0, "mode": "check"}}});
    let project = "/admin/orgs/acme/projects/bot";
    let pause = "/admin/orgs/acme/runs/r1/pause";
    let (status, body) = t
        .as_user(&vera, Method::PUT, project, Some(budgets.clone()))
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(
        body["error"]
            .as_str()
            .unwrap()
            .contains("needs the admin role"),
        "{body}"
    );
    assert_eq!(
        t.as_user(&vera, Method::POST, pause, None).await.0,
        StatusCode::FORBIDDEN
    );
    let new_org = Some(json!({"id": "x"}));
    assert_eq!(
        t.as_user(&vera, Method::POST, "/admin/orgs", new_org)
            .await
            .0,
        StatusCode::FORBIDDEN
    );

    // Without the right CSRF header a session cannot change anything.
    let forged = Sess {
        csrf: "wrong".into(),
        ..vera.clone()
    };
    let (status, body) = t.as_user(&forged, Method::POST, "/auth/logout", None).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");

    // Roles take effect on the next request.
    let member = format!("/admin/orgs/acme/members/{vid}");
    let role = |r: &str| Some(json!({ "role": r }));
    assert_eq!(
        t.root(Method::PUT, &member, role("approver")).await.0,
        StatusCode::OK
    );
    assert_eq!(
        t.as_user(&vera, Method::POST, pause, None).await.0,
        StatusCode::OK
    );
    assert_eq!(
        t.root(Method::PUT, &member, role("admin")).await.0,
        StatusCode::OK
    );
    assert_eq!(
        t.as_user(&vera, Method::PUT, project, Some(budgets))
            .await
            .0,
        StatusCode::OK
    );
    // Admins add members up to admin, but cannot make owners.
    let members = "/admin/orgs/acme/members";
    let owner = Some(json!({"email": "o@example.com", "role": "owner"}));
    assert_eq!(
        t.as_user(&vera, Method::POST, members, owner).await.0,
        StatusCode::FORBIDDEN
    );
    let ann = Some(json!({"email": "ann@example.com", "role": "viewer"}));
    let (status, added) = t.as_user(&vera, Method::POST, members, ann).await;
    assert_eq!(status, StatusCode::CREATED, "{added}");
    // ...and see only their org's audit log.
    assert_eq!(t.get(&vera, "/admin/audit").await.0, StatusCode::FORBIDDEN);
    let (status, log) = t.get(&vera, "/admin/audit?org=acme&limit=3").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(actions(&log)[..2], ["member.add", "project.update"]);
    assert_eq!(log["entries"][0]["actor"], "vera@example.com");
    assert_eq!(log["entries"][1]["actor"], "vera@example.com");

    // An org with owners keeps at least one.
    let (oid, _olga) = t.member("acme", "olga@example.com", "owner").await;
    let olga = format!("/admin/orgs/acme/members/{oid}");
    let (status, body) = t.root(Method::DELETE, &olga, None).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(
        t.as_user(&vera, Method::DELETE, &olga, None).await.0,
        StatusCode::FORBIDDEN
    );

    // Removing someone ends their access at once.
    assert_eq!(
        t.root(Method::DELETE, &member, None).await.0,
        StatusCode::NO_CONTENT
    );
    let (_, ov) = t.get(&vera, "/admin/overview").await;
    assert!(org_ids(&ov).is_empty());
}

#[tokio::test]
async fn passwords_sessions_and_lockout() {
    let Some(t) = T::start().await else { return };
    let body = json!({"email": "pat@example.com", "role": "viewer"});
    let (status, added) = t
        .root(Method::POST, "/admin/orgs/acme/members", Some(body))
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let token = setup_token(&added);
    let (status, body) = t.setup(&token, "short").await.err().unwrap();
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    // A weak password does not use up the link; a link works once.
    let sess = t.setup(&token, PASSWORD).await.unwrap();
    let again = t.setup(&token, PASSWORD).await.err().unwrap();
    assert_eq!(again.0, StatusCode::BAD_REQUEST);

    let failed = |r: Result<Sess, (StatusCode, Value)>| r.err().unwrap().0;
    assert_eq!(
        failed(t.login("pat@example.com", "not the password").await),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        failed(t.login("nobody@example.com", PASSWORD).await),
        StatusCode::UNAUTHORIZED
    );
    let second = t.login("PAT@example.com", PASSWORD).await.unwrap();
    assert_ne!(second.cookie, sess.cookie);

    // Signing out ends that session only.
    assert_eq!(
        t.as_user(&second, Method::POST, "/auth/logout", None)
            .await
            .0,
        StatusCode::NO_CONTENT
    );
    assert_eq!(t.get(&second, "/auth/me").await.0, StatusCode::UNAUTHORIZED);
    assert_eq!(t.get(&sess, "/auth/me").await.0, StatusCode::OK);

    // A new password ends the other sessions.
    let other = t.login("pat@example.com", PASSWORD).await.unwrap();
    let change = json!({"current": PASSWORD, "new": "another long passphrase"});
    assert_eq!(
        t.as_user(&sess, Method::PUT, "/auth/password", Some(change))
            .await
            .0,
        StatusCode::NO_CONTENT
    );
    assert_eq!(t.get(&other, "/auth/me").await.0, StatusCode::UNAUTHORIZED);
    assert_eq!(t.get(&sess, "/auth/me").await.0, StatusCode::OK);

    // Ten wrong passwords lock the account, even for the right one.
    for _ in 0..10 {
        let _ = t.login("pat@example.com", "wrong password here").await;
    }
    assert_eq!(
        failed(t.login("pat@example.com", "another long passphrase").await),
        StatusCode::TOO_MANY_REQUESTS
    );

    // Disabled accounts lose their sessions.
    let (_, users) = t.root(Method::GET, "/admin/users", None).await;
    let pat = users["users"]
        .as_array()
        .unwrap()
        .iter()
        .find(|u| u["email"] == "pat@example.com")
        .unwrap()
        .clone();
    assert_eq!(pat["roles"]["acme"], "viewer");
    assert_eq!(pat["has_password"], true);
    let path = format!("/admin/users/{}", pat["id"]);
    let off = Some(json!({"disabled": true}));
    assert_eq!(
        t.root(Method::PUT, &path, off).await.0,
        StatusCode::NO_CONTENT
    );
    assert_eq!(t.get(&sess, "/auth/me").await.0, StatusCode::UNAUTHORIZED);

    // Sign-ins are in the audit log, never passwords.
    let (_, log) = t.root(Method::GET, "/admin/audit?limit=50", None).await;
    let text = log.to_string();
    assert!(!text.contains(PASSWORD) && !text.contains("another long"));
    let seen = actions(&log);
    for a in [
        "auth.login",
        "auth.login_failed",
        "auth.logout",
        "auth.password_set",
        "auth.password_changed",
        "user.update",
    ] {
        assert!(seen.contains(&a), "{a} missing from {seen:?}");
    }
}

#[tokio::test]
async fn api_tokens_act_as_their_user() {
    let Some(t) = T::start().await else { return };
    let (_, sess) = t.member("acme", "bot-owner@example.com", "admin").await;
    // The admin token is not a person and has no API tokens.
    let body = Some(json!({"name": "x"}));
    assert_eq!(
        t.root(Method::POST, "/auth/tokens", body).await.0,
        StatusCode::BAD_REQUEST
    );
    let body = Some(json!({"name": "ci", "expires_days": 30}));
    let (status, created) = t.as_user(&sess, Method::POST, "/auth/tokens", body).await;
    assert_eq!(status, StatusCode::CREATED);
    let token = created["token"].as_str().unwrap().to_string();
    assert!(token.starts_with("tlu_"));

    let api = |method: Method, path: &str| {
        t.http
            .request(method, format!("{}{path}", t.url))
            .bearer_auth(&token)
    };
    let (status, ov) = t.send(api(Method::GET, "/admin/overview"), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(org_ids(&ov), ["acme"]);
    // Tokens need no CSRF header: a browser never sends them on its own.
    let body = Some(json!({"id": "from-ci"}));
    let projects = api(Method::POST, "/admin/orgs/acme/projects");
    assert_eq!(t.send(projects, body).await.0, StatusCode::CREATED);
    let (_, log) = t.root(Method::GET, "/admin/audit?limit=1", None).await;
    assert_eq!(log["entries"][0]["actor"], "bot-owner@example.com");

    let (_, list) = t.as_user(&sess, Method::GET, "/auth/tokens", None).await;
    assert_eq!(list["tokens"][0]["name"], "ci");
    assert!(list["tokens"][0]["last_used_at"].is_string());
    assert!(!list.to_string().contains(&token));
    let id = created["id"].as_i64().unwrap();
    let revoke = format!("/auth/tokens/{id}");
    assert_eq!(
        t.as_user(&sess, Method::DELETE, &revoke, None).await.0,
        StatusCode::NO_CONTENT
    );
    let overview = api(Method::GET, "/admin/overview");
    assert_eq!(t.send(overview, None).await.0, StatusCode::UNAUTHORIZED);
}

// --- Single sign-on against a mock identity provider ---

#[derive(Default)]
struct Idp {
    url: String,
    /// What the next ID token says: claims merged over the defaults.
    claims: Value,
    /// code -> (PKCE challenge, nonce) from the authorization request.
    codes: HashMap<String, (String, String)>,
    /// The Authorization header the token endpoint received last.
    auth: Option<String>,
}
type SharedIdp = Arc<Mutex<Idp>>;

async fn discovery(State(idp): State<SharedIdp>) -> Json<Value> {
    let url = idp.lock().unwrap().url.clone();
    Json(json!({
        "issuer": url,
        "authorization_endpoint": format!("{url}/authorize"),
        "token_endpoint": format!("{url}/token"),
        "jwks_uri": format!("{url}/jwks"),
    }))
}

async fn jwks() -> Json<Value> {
    let n = include_str!("fixtures/oidc_test_rsa.n").trim();
    Json(
        json!({"keys": [{"kty": "RSA", "kid": "test-key", "use": "sig", "alg": "RS256", "n": n, "e": "AQAB"}]}),
    )
}

async fn token(
    State(idp): State<SharedIdp>,
    headers: axum::http::HeaderMap,
    Form(form): Form<HashMap<String, String>>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    use base64::Engine;
    use sha2::Digest;
    let bad = || {
        let body = Json(json!({"error": "invalid_grant"}));
        (axum::http::StatusCode::BAD_REQUEST, body).into_response()
    };
    let mut idp = idp.lock().unwrap();
    idp.auth = headers
        .get("authorization")
        .map(|v| v.to_str().unwrap().to_string());
    let code = form.get("code").cloned().unwrap_or_default();
    let Some((challenge, nonce)) = idp.codes.remove(&code) else {
        return bad();
    };
    let verifier = form.get("code_verifier").cloned().unwrap_or_default();
    let expected = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(sha2::Sha256::digest(verifier.as_bytes()));
    let redirect = format!("{PUBLIC_URL}/auth/sso/callback");
    if expected != challenge || form.get("redirect_uri") != Some(&redirect) {
        return bad();
    }
    let now = chrono::Utc::now().timestamp();
    let mut claims = json!({
        "iss": idp.url, "aud": "tokenlens-client", "sub": "user-1", "iat": now, "exp": now + 300,
        "nonce": nonce, "email": "sam@example.com", "email_verified": true, "name": "Sam Lee",
        "groups": ["tl-admins"],
    });
    for (k, v) in idp.claims.as_object().cloned().unwrap_or_default() {
        claims[k] = v;
    }
    let mut header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::RS256);
    header.kid = Some("test-key".into());
    let pem = include_bytes!("fixtures/oidc_test_rsa.pem");
    let key = jsonwebtoken::EncodingKey::from_rsa_pem(pem).unwrap();
    let id_token = jsonwebtoken::encode(&header, &claims, &key).unwrap();
    Json(json!({"access_token": "at", "token_type": "Bearer", "id_token": id_token}))
        .into_response()
}

impl T {
    /// Start a sign-in, "approve" it at the provider, and come back to the callback.
    /// Returns where the callback redirects to, and the session cookie it set.
    async fn sso(&self, idp: &SharedIdp) -> (String, Option<String>) {
        let start = self
            .http
            .get(format!("{}/auth/sso/start", self.url))
            .send()
            .await
            .unwrap();
        assert_eq!(start.status(), StatusCode::FOUND);
        let to = url::Url::parse(start.headers()["location"].to_str().unwrap()).unwrap();
        let q: HashMap<String, String> = to.query_pairs().into_owned().collect();
        assert_eq!(q["code_challenge_method"], "S256");
        assert_eq!(q["redirect_uri"], format!("{PUBLIC_URL}/auth/sso/callback"));
        assert!(q["scope"].contains("groups"));
        let code = uuid::Uuid::new_v4().to_string();
        let pending = (q["code_challenge"].clone(), q["nonce"].clone());
        idp.lock().unwrap().codes.insert(code.clone(), pending);
        let back = self
            .http
            .get(format!(
                "{}/auth/sso/callback?code={code}&state={}",
                self.url, q["state"]
            ))
            .send()
            .await
            .unwrap();
        assert_eq!(back.status(), StatusCode::FOUND);
        let location = back.headers()["location"].to_str().unwrap().to_string();
        (location, session_cookie(&back))
    }
}

fn decode(s: &str) -> String {
    url::form_urlencoded::parse(format!("m={s}").as_bytes())
        .next()
        .unwrap()
        .1
        .into_owned()
}

#[tokio::test]
async fn single_sign_on_with_group_roles() {
    let Some(t) = T::start().await else { return };
    let idp: SharedIdp = Arc::default();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let issuer = format!("http://{}", listener.local_addr().unwrap());
    idp.lock().unwrap().url = issuer.clone();
    let app = Router::new()
        .route("/.well-known/openid-configuration", get(discovery))
        .route("/jwks", get(jwks))
        .route("/token", post(token))
        .with_state(idp.clone());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let sso = json!({
        "enabled": true, "label": "Acme SSO", "issuer": issuer, "client_id": "tokenlens-client",
        "client_secret": "s3cret-value", "allowed_domains": ["example.com"], "auto_create": true,
        "group_roles": [{"group": "tl-admins", "org": "acme", "role": "admin"},
                        {"group": "tl-viewers", "org": "other", "role": "viewer"}],
    });
    let (status, saved) = t.root(Method::PUT, "/admin/sso", Some(sso)).await;
    assert_eq!(status, StatusCode::OK, "{saved}");
    assert_eq!(saved["config"]["client_secret"], json!({"last4": "alue"}));
    let (_, check) = t.root(Method::POST, "/admin/sso/test", None).await;
    assert_eq!(check["ok"], true, "{check}");
    let (_, cfg) = t.root(Method::GET, "/auth/config", None).await;
    assert_eq!(cfg["sso"]["label"], "Acme SSO");

    // The first sign-in creates the account, with the roles its groups give.
    let (to, cookie) = t.sso(&idp).await;
    assert_eq!(to, "/");
    let sess = Sess {
        cookie: cookie.unwrap(),
        csrf: String::new(),
    };
    let (_, me) = t.as_user(&sess, Method::GET, "/auth/me", None).await;
    assert_eq!(me["email"], "sam@example.com");
    assert_eq!(me["roles"], json!({"acme": "admin"}));
    let auth = idp.lock().unwrap().auth.clone().unwrap();
    assert!(
        auth.starts_with("Basic "),
        "client secret sent with HTTP Basic"
    );

    // Groups are synced at every sign-in.
    idp.lock().unwrap().claims = json!({"groups": ["tl-viewers"]});
    let (_, cookie) = t.sso(&idp).await;
    let sess = Sess {
        cookie: cookie.unwrap(),
        csrf: String::new(),
    };
    let (_, me) = t.as_user(&sess, Method::GET, "/auth/me", None).await;
    assert_eq!(me["roles"], json!({"other": "viewer"}));

    // Refused: other domains, unverified emails, tokens meant for another client.
    for (claims, message) in [
        (json!({"email": "eve@evil.test"}), "cannot sign in here"),
        (json!({"email_verified": false}), "not verified"),
        (json!({"aud": "someone-else"}), "not valid"),
    ] {
        idp.lock().unwrap().claims = claims;
        let (to, cookie) = t.sso(&idp).await;
        assert!(cookie.is_none());
        assert!(to.starts_with("/#login-error="), "{to}");
        let msg = decode(to.trim_start_matches("/#login-error="));
        assert!(msg.contains(message), "{msg}");
    }
    // A sign-in's state works once, and made-up ones not at all.
    let back = t
        .http
        .get(format!("{}/auth/sso/callback?code=x&state=made-up", t.url))
        .send()
        .await
        .unwrap();
    let to = back.headers()["location"].to_str().unwrap();
    assert!(decode(to).contains("expired"), "{to}");

    let (_, log) = t.root(Method::GET, "/admin/audit?limit=20", None).await;
    let seen = actions(&log);
    assert!(
        seen.contains(&"sso.update") && seen.contains(&"auth.sso_failed"),
        "{seen:?}"
    );
    assert!(!log.to_string().contains("s3cret"));
}

#[tokio::test]
async fn without_a_database_only_the_admin_token_works() {
    let Some(redis) = common::RedisServer::start() else {
        return;
    };
    let rt = Resolved::from_toml(&config(&redis.url), |name| match name {
        "ADMIN" => Some(ADMIN_TOKEN.into()),
        _ => None,
    })
    .unwrap();
    let state = build_state(rt).await.unwrap();
    control::init(&state).await.unwrap();
    let url = serve(state).await;
    let http = reqwest::Client::new();
    let cfg: Value = http
        .get(format!("{url}/auth/config"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let expected =
        json!({"database": false, "password_login": false, "admin_token": true, "sso": null});
    assert_eq!(cfg, expected);
    let login = json!({"email": "a@b.co", "password": PASSWORD});
    let resp = http
        .post(format!("{url}/auth/login"))
        .json(&login)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CONFLICT);
    let resp = http
        .get(format!("{url}/admin/overview"))
        .header("cookie", "tl_session=abc")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}
