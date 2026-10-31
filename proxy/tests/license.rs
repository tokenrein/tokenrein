//! License keys: what the free tier refuses, what a key unlocks, and how usage is counted.

mod common;

use std::sync::Arc;

use jsonwebtoken::{Algorithm, EncodingKey, Header};
use reqwest::StatusCode;
use serde_json::{Value, json};
use tokenrein::config::Resolved;
use tokenrein::ledger::Keys;
use tokenrein::license::{Claims, Plan};
use tokenrein::{AppState, admin, build_state, control};

const ADMIN_TOKEN: &str = "admin-token-0123456789";
const SECRETS: &str = "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f";
use common::{LICENSE_TEST_PRIVATE as TEST_PRIVATE, LICENSE_TEST_PUBLIC as TEST_PUBLIC};

fn key(plan: Plan, days_left: i64) -> String {
    let now = chrono::Utc::now().timestamp();
    let claims = Claims {
        iss: "tokenrein".into(),
        sub: "Acme".into(),
        plan,
        developers: Some(50),
        iat: now,
        exp: now + days_left * 86_400,
        jti: "lic_test".into(),
    };
    let signer = EncodingKey::from_ed_pem(TEST_PRIVATE.as_bytes()).unwrap();
    jsonwebtoken::encode(&Header::new(Algorithm::EdDSA), &claims, &signer).unwrap()
}

async fn instance(redis: &str, db: Option<&str>) -> Arc<AppState> {
    let db = db.map(str::to_string);
    let history = std::env::temp_dir().join(format!("tokenrein-license-{}", rand_suffix()));
    let history = history.display();
    let config = format!(
        r#"
        [server]
        listen = "127.0.0.1:0"
        admin_token_env = "ADMIN"
        secrets_key_env = "SECRETS"
        [redis]
        url = "{redis}"
        [database]
        url_env = "DB"
        [history]
        dir = "{history}"
        [[orgs]]
        id = "acme"
        [[orgs.projects]]
        id = "bot"
        "#
    );
    let rt = Resolved::from_toml(&config, move |name| match name {
        "ADMIN" => Some(ADMIN_TOKEN.into()),
        "SECRETS" => Some(SECRETS.into()),
        "DB" => db.clone(),
        "TOKENREIN_LICENSE_TEST_PUBLIC_KEY" => Some(TEST_PUBLIC.into()),
        _ => None,
    })
    .unwrap();
    let state = build_state(rt).await.unwrap();
    control::init(&state).await.unwrap();
    state
}

fn rand_suffix() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("{}-{nanos}", std::process::id())
}

async fn serve_admin(state: Arc<AppState>) -> String {
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

struct Api {
    http: reqwest::Client,
    url: String,
}

impl Api {
    async fn send(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        let mut req = self
            .http
            .request(method, format!("{}{path}", self.url))
            .bearer_auth(ADMIN_TOKEN);
        if let Some(b) = body {
            req = req.json(&b);
        }
        let resp = req.send().await.unwrap();
        let status = resp.status();
        let text = resp.text().await.unwrap();
        (
            status,
            serde_json::from_str(&text).unwrap_or(Value::String(text)),
        )
    }
    async fn get(&self, path: &str) -> (StatusCode, Value) {
        self.send(reqwest::Method::GET, path, None).await
    }
    async fn post(&self, path: &str, body: Value) -> (StatusCode, Value) {
        self.send(reqwest::Method::POST, path, Some(body)).await
    }
    async fn put(&self, path: &str, body: Value) -> (StatusCode, Value) {
        self.send(reqwest::Method::PUT, path, Some(body)).await
    }
}

fn license_required(r: &(StatusCode, Value), feature: &str) {
    assert_eq!(r.0, StatusCode::FORBIDDEN, "{feature}: {}", r.1);
    assert_eq!(r.1["code"], "license_required", "{feature}: {}", r.1);
    assert_eq!(r.1["feature"], feature);
}

#[tokio::test]
async fn the_free_tier_refuses_paid_features_and_a_key_unlocks_them() {
    let Some(redis) = common::RedisServer::start() else {
        return;
    };
    let state = instance(&redis.url, None).await;
    let api = Api {
        http: reqwest::Client::new(),
        url: serve_admin(state.clone()).await,
    };

    let (status, lic) = api.get("/admin/license").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(lic["standing"], "free");
    assert_eq!(lic["limits"]["developers"], 25);
    assert_eq!(lic["features"]["sso"]["allowed"], false);

    let second_org = json!({ "id": "beta" });
    let pagerduty =
        json!({ "name": "On call", "kind": "pagerduty", "target": "abcdefghij0123456789abcd" });
    let cursor = json!({ "project": "bot", "api_key": "key_0123456789abcdef0123456789abcdef" });
    license_required(
        &api.post("/admin/orgs", second_org.clone()).await,
        "multiple_orgs",
    );
    license_required(&api.get("/admin/audit?format=csv").await, "audit_export");
    license_required(
        &api.get("/admin/orgs/acme/spend?days=120").await,
        "long_history",
    );
    license_required(
        &api.post("/admin/orgs/acme/alerts", pagerduty.clone()).await,
        "pagerduty",
    );
    license_required(
        &api.put("/admin/orgs/acme/connectors/cursor", cursor).await,
        "cursor_import",
    );
    // What stays free still works.
    assert_eq!(
        api.get("/admin/orgs/acme/spend?days=90").await.0,
        StatusCode::OK
    );
    assert_eq!(api.get("/admin/audit").await.0, StatusCode::OK);

    // A key that does not verify is refused.
    let (status, _) = api
        .put("/admin/license", json!({ "key": "not-a-key" }))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // Team unlocks everything but a second org.
    let (status, lic) = api
        .put("/admin/license", json!({ "key": key(Plan::Team, 365) }))
        .await;
    assert_eq!(status, StatusCode::OK, "{lic}");
    assert_eq!(lic["standing"], "licensed");
    assert_eq!(lic["plan"], "team");
    assert_eq!(lic["customer"], "Acme");
    assert_eq!(lic["source"], "dashboard");
    assert_eq!(api.get("/admin/audit?format=csv").await.0, StatusCode::OK);
    assert_eq!(
        api.get("/admin/orgs/acme/spend?days=120").await.0,
        StatusCode::OK
    );
    assert!(
        api.post("/admin/orgs/acme/alerts", pagerduty)
            .await
            .0
            .is_success()
    );
    license_required(
        &api.post("/admin/orgs", second_org.clone()).await,
        "multiple_orgs",
    );

    // Enterprise adds orgs.
    api.put(
        "/admin/license",
        json!({ "key": key(Plan::Enterprise, 365) }),
    )
    .await;
    let (status, body) = api.post("/admin/orgs", second_org).await;
    assert!(status.is_success(), "{status} {body}");

    // The key is redacted in the audit log.
    let (_, audit) = api.get("/admin/audit").await;
    let text = audit.to_string();
    assert!(text.contains("license.update"), "{text}");
    assert!(
        !text.contains("eyJ"),
        "a key leaked into the audit log: {text}"
    );

    // Removing it goes back to free; the orgs already made keep working.
    let (status, _) = api
        .send(reqwest::Method::DELETE, "/admin/license", None)
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(api.get("/admin/license").await.1["standing"], "free");
    assert_eq!(state.rt().cfg.orgs.len(), 2);
}

#[tokio::test]
async fn an_expired_key_keeps_working_through_the_grace_period() {
    let Some(redis) = common::RedisServer::start() else {
        return;
    };
    let state = instance(&redis.url, None).await;
    let api = Api {
        http: reqwest::Client::new(),
        url: serve_admin(state).await,
    };
    api.put("/admin/license", json!({ "key": key(Plan::Team, -10) }))
        .await;
    let (_, lic) = api.get("/admin/license").await;
    assert_eq!(lic["standing"], "grace");
    assert_eq!(api.get("/admin/audit?format=csv").await.0, StatusCode::OK);

    api.put("/admin/license", json!({ "key": key(Plan::Team, -31) }))
        .await;
    let (_, lic) = api.get("/admin/license").await;
    assert_eq!(lic["standing"], "expired");
    license_required(&api.get("/admin/audit?format=csv").await, "audit_export");
}

#[tokio::test]
async fn developers_and_spend_are_counted_from_the_last_month() {
    let Some(redis) = common::RedisServer::start() else {
        return;
    };
    let state = instance(&redis.url, None).await;
    let now = chrono::Utc::now();
    let keys = Keys::new("acme");
    let mut conn = state.ledger.conn();
    let today = keys.spend_day(&now.format("%Y%m%d").to_string());
    let old = keys.spend_day(
        &(now - chrono::Duration::days(40))
            .format("%Y%m%d")
            .to_string(),
    );
    let mut pipe = redis::pipe();
    for i in 0..30 {
        pipe.hset(&today, format!("user:dev{i}@acme.test:n"), 1)
            .ignore();
    }
    // Calls without a person, and people only seen before the window, do not count.
    pipe.hset(&today, "user:-:n", 5).ignore();
    pipe.hset(&old, "user:gone@acme.test:n", 1).ignore();
    pipe.hset(&today, "project:bot:c", 2_500_000_i64).ignore();
    let _: () = pipe.query_async(&mut conn).await.unwrap();

    let usage = tokenrein::license::measure(&state, now).await.unwrap();
    assert_eq!(usage.developers, 30);
    assert_eq!(usage.spend_micros, 2_500_000);
    let status = tokenrein::license::status(&state).await.unwrap();
    assert_eq!(
        status["over_limit"], true,
        "30 developers is over the free 25"
    );
}

#[tokio::test]
async fn single_sign_on_needs_a_key_but_can_always_be_turned_off() {
    let Some(redis) = common::RedisServer::start() else {
        return;
    };
    let Some(pg) = common::PostgresServer::start() else {
        return;
    };
    let state = instance(&redis.url, Some(&pg.url)).await;
    let api = Api {
        http: reqwest::Client::new(),
        url: serve_admin(state).await,
    };
    let sso = |enabled: bool| {
        json!({
            "enabled": enabled,
            "issuer": "https://idp.example.com",
            "client_id": "tokenrein",
            "client_secret": "s3cret-value-for-tests",
        })
    };
    license_required(&api.put("/admin/sso", sso(true)).await, "sso");
    assert_eq!(api.put("/admin/sso", sso(false)).await.0, StatusCode::OK);
    let (_, cfg) = api.get("/auth/config").await;
    assert_eq!(cfg["sso"], Value::Null);
}
