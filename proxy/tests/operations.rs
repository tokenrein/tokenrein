//! Running Tokenrein: Prometheus metrics, admin security headers and secrets key rotation.

mod common;

use std::sync::Arc;

use reqwest::StatusCode;
use serde_json::{Value, json};
use tokenrein::config::Resolved;
use tokenrein::providers::Provider;
use tokenrein::{AppState, admin, build_state, control};

const ADMIN_TOKEN: &str = "admin-token-0123456789";
const K1: &str = "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f";
const K2: &str = "1f1e1d1c1b1a191817161514131211100f0e0d0c0b0a09080706050403020100";
const K3: &str = "2222222222222222222222222222222222222222222222222222222222222222";
// Not a real key: only has to look like one.
const PROVIDER_KEY: &str = "sk-test-rotation-0123456789";

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
        [providers.openai]
        base_url = "http://127.0.0.1:9"
        api_key_env = "OPENAI_KEY"
        [[orgs]]
        id = "acme"
        [[orgs.projects]]
        id = "bot"
        "#
    )
}

/// A proxy instance with the given secrets key and, during a rotation, the previous one.
async fn instance(
    redis: &str,
    db: Option<&str>,
    secrets: &'static str,
    previous: Option<&'static str>,
) -> Arc<AppState> {
    let db = db.map(str::to_string);
    let rt = Resolved::from_toml(&config(redis), move |name| match name {
        "ADMIN" => Some(ADMIN_TOKEN.into()),
        "SECRETS" => Some(secrets.into()),
        "SECRETS_PREVIOUS" => previous.map(str::to_string),
        "DB" => db.clone(),
        _ => None,
    })
    .unwrap();
    let state = build_state(rt).await.unwrap();
    control::init(&state).await.unwrap();
    state
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

#[tokio::test]
async fn metrics_are_served_and_every_admin_route_sends_security_headers() {
    let Some(redis) = common::RedisServer::start() else {
        return;
    };
    let state = instance(&redis.url, None, K1, None).await;
    let url = serve_admin(state).await;
    let http = reqwest::Client::new();

    let resp = http.get(format!("{url}/metrics")).send().await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(
        resp.headers()["content-type"]
            .to_str()
            .unwrap()
            .starts_with("text/plain; version=0.0.4")
    );
    let body = resp.text().await.unwrap();
    for metric in [
        "tokenrein_decisions_total{path=\"proxy\",decision=\"allowed\"}",
        "tokenrein_pre_upstream_seconds_bucket{le=\"0.001\"}",
        "tokenrein_upstream_seconds_count{provider=\"anthropic\"}",
        "tokenrein_ledger_errors_total",
        "tokenrein_activity_events_dropped_total{reason=\"queue_full\"}",
        "tokenrein_audit_write_failures_total",
        "tokenrein_config_version 1\n",
    ] {
        assert!(body.contains(metric), "missing {metric} in\n{body}");
    }

    // API, health, metrics, and errors alike.
    for (path, auth, status) in [
        ("/healthz", false, StatusCode::OK),
        ("/metrics", false, StatusCode::OK),
        ("/admin/overview", false, StatusCode::UNAUTHORIZED),
        ("/admin/overview", true, StatusCode::OK),
        ("/nope", false, StatusCode::NOT_FOUND),
    ] {
        let mut req = http.get(format!("{url}{path}"));
        if auth {
            req = req.bearer_auth(ADMIN_TOKEN);
        }
        let resp = req.send().await.unwrap();
        assert_eq!(resp.status(), status, "{path}");
        let h = resp.headers();
        assert_eq!(h["x-content-type-options"], "nosniff", "{path}");
        assert_eq!(h["referrer-policy"], "no-referrer", "{path}");
        assert_eq!(h["x-frame-options"], "DENY", "{path}");
        assert!(
            h["content-security-policy"]
                .to_str()
                .unwrap()
                .contains("frame-ancestors 'none'"),
            "{path}"
        );
    }
    // The dashboard keeps its own, stricter policy.
    let resp = http.get(format!("{url}/")).send().await.unwrap();
    let csp = resp.headers()["content-security-policy"].to_str().unwrap();
    assert!(csp.starts_with("default-src 'none'"), "{csp}");
}

async fn rotates_saved_provider_keys(redis: &str, db: Option<&str>) {
    // A key saved from the dashboard under K1.
    let old = instance(redis, db, K1, None).await;
    let url = serve_admin(old).await;
    let resp = reqwest::Client::new()
        .put(format!("{url}/admin/orgs/acme/provider-keys/openai"))
        .bearer_auth(ADMIN_TOKEN)
        .json(&json!({ "api_key": PROVIDER_KEY }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Instances switched to K2 keep working while the previous key is set...
    let rolling = instance(redis, db, K2, Some(K1)).await;
    assert_eq!(
        rolling.rt().provider_key(0, Provider::OpenAI),
        Some(PROVIDER_KEY)
    );
    // ...but not without it.
    let new_only = instance(redis, db, K2, None).await;
    assert_eq!(new_only.rt().provider_key(0, Provider::OpenAI), None);

    // A wrong previous key changes nothing.
    let wrong = instance(redis, db, K3, Some(K2)).await;
    let e = control::rotate_secrets_key(&wrong, "test")
        .await
        .unwrap_err();
    assert!(format!("{e:#}").contains("acme/openai"), "{e:#}");

    let resealed = control::rotate_secrets_key(&rolling, "test").await.unwrap();
    assert_eq!(resealed, vec!["acme/openai".to_string()]);
    // Running it again finds nothing left to do.
    assert!(
        control::rotate_secrets_key(&rolling, "test")
            .await
            .unwrap()
            .is_empty()
    );

    // After the rotation, K2 alone opens the saved key.
    let after = instance(redis, db, K2, None).await;
    assert_eq!(
        after.rt().provider_key(0, Provider::OpenAI),
        Some(PROVIDER_KEY)
    );
    assert_eq!(
        after
            .rt()
            .provider_status(0, Provider::OpenAI)
            .unwrap()
            .source,
        Some("dashboard")
    );

    // The rotation is in the audit log, without the sealed values.
    let url = serve_admin(after).await;
    let page: Value = reqwest::Client::new()
        .get(format!("{url}/admin/audit"))
        .bearer_auth(ADMIN_TOKEN)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let entry = &page["entries"][0];
    assert_eq!(entry["action"], "secrets_key.rotate", "{page}");
    assert_eq!(entry["actor"], "test");
    assert_eq!(entry["detail"]["resealed"], json!(["acme/openai"]));
}

#[tokio::test]
async fn rotates_the_secrets_key_with_redis_only() {
    let Some(redis) = common::RedisServer::start() else {
        return;
    };
    rotates_saved_provider_keys(&redis.url, None).await;
}

#[tokio::test]
async fn rotates_the_secrets_key_with_postgres() {
    let Some(redis) = common::RedisServer::start() else {
        return;
    };
    let Some(pg) = common::PostgresServer::start() else {
        return;
    };
    rotates_saved_provider_keys(&redis.url, Some(&pg.url)).await;
}

// --- Outages: what keeps working when Redis or Postgres is down ---

const OPEN_KEY: &str = "tl_test_fails_open_0001";
const CLOSED_KEY: &str = "tl_test_fails_closed_0001";

/// An OpenAI stand-in that answers every chat completion.
async fn fake_openai() -> String {
    let app = axum::Router::new().route(
        "/v1/chat/completions",
        axum::routing::post(|| async {
            axum::Json(json!({
                "id": "c1",
                "object": "chat.completion",
                "choices": [{"index": 0, "message": {"role": "assistant", "content": "hi"}}],
                "usage": {"prompt_tokens": 10, "completion_tokens": 5}
            }))
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}")
}

/// Two orgs, one failing open and one failing closed, served by a proxy and an admin listener.
struct Outage {
    proxy: String,
    admin: String,
    http: reqwest::Client,
}

impl Outage {
    async fn start(redis: &str, db: Option<&str>) -> Self {
        let upstream = fake_openai().await;
        let hash = tokenrein::config::sha256_hex;
        let config = format!(
            r#"
            [server]
            listen = "127.0.0.1:0"
            admin_token_env = "ADMIN"
            [redis]
            url = "{redis}"
            [database]
            url_env = "DB"
            [providers.openai]
            base_url = "{upstream}"
            api_key_env = "OPENAI_KEY"
            [[orgs]]
            id = "open"
            fail_mode = "open"
            [[orgs.projects]]
            id = "bot"
            virtual_key_hashes = ["{}"]
            [[orgs]]
            id = "shut"
            fail_mode = "closed"
            [[orgs.projects]]
            id = "bot"
            virtual_key_hashes = ["{}"]
            "#,
            hash(OPEN_KEY),
            hash(CLOSED_KEY)
        );
        let db = db.map(str::to_string);
        let rt = Resolved::from_toml(&config, move |name| match name {
            "ADMIN" => Some(ADMIN_TOKEN.into()),
            "OPENAI_KEY" => Some("sk-fake-upstream-key".into()),
            "DB" => db.clone(),
            _ => None,
        })
        .unwrap();
        let state = build_state(rt).await.unwrap();
        control::init(&state).await.unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy = format!("http://{}", listener.local_addr().unwrap());
        let router = tokenrein::proxy::router(state.clone());
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        Self {
            proxy,
            admin: serve_admin(state).await,
            http: reqwest::Client::new(),
        }
    }

    async fn chat(&self, key: &str) -> reqwest::Response {
        self.http
            .post(format!("{}/v1/chat/completions", self.proxy))
            .bearer_auth(key)
            .header("x-tr-run-id", "outage-run")
            .json(&json!({"model": "gpt-4o-mini", "messages": [{"role": "user", "content": "hi"}]}))
            .send()
            .await
            .unwrap()
    }

    async fn metric(&self, line: &str) -> u64 {
        let body = self
            .http
            .get(format!("{}/metrics", self.admin))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        body.lines()
            .find_map(|l| l.strip_prefix(line)?.trim().parse().ok())
            .unwrap_or_else(|| panic!("no {line} in\n{body}"))
    }
}

#[tokio::test]
async fn redis_outage_fails_open_or_closed_per_org() {
    let Some(redis) = common::RedisServer::start() else {
        return;
    };
    let Some(pg) = common::PostgresServer::start() else {
        return;
    };
    let o = Outage::start(&redis.url, Some(&pg.url)).await;
    let resp = o.chat(OPEN_KEY).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(resp.headers().get("x-tr-degraded").is_none());
    let errors_before = o.metric("tokenrein_ledger_errors_total").await;

    drop(redis);

    // Fails open: the call goes through, unmetered and flagged, and without waiting on Redis.
    let started = std::time::Instant::now();
    let resp = o.chat(OPEN_KEY).await;
    let took = started.elapsed();
    eprintln!("fail-open request with Redis down took {took:?}");
    assert!(took < std::time::Duration::from_secs(3), "{took:?}");
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(resp.headers()["x-tr-degraded"], "true");
    // Fails closed: refused.
    let resp = o.chat(CLOSED_KEY).await;
    assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["code"], "tokenrein_unavailable", "{body}");

    let readyz = o
        .http
        .get(format!("{}/readyz", o.admin))
        .send()
        .await
        .unwrap();
    assert_eq!(readyz.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert!(o.metric("tokenrein_ledger_errors_total").await > errors_before);
    // The config lives in Postgres, so it can still be changed.
    let edit = o
        .http
        .put(format!("{}/admin/orgs/open/projects/bot", o.admin))
        .bearer_auth(ADMIN_TOKEN)
        .json(&json!({"budgets": {"run": {"limit_usd": 1.0, "mode": "enforce"}}}))
        .send()
        .await
        .unwrap();
    assert_eq!(edit.status(), StatusCode::OK);
    assert!(
        o.metric("tokenrein_decisions_total{path=\"proxy\",decision=\"allowed_degraded\"}")
            .await
            >= 1
    );
    assert!(
        o.metric("tokenrein_decisions_total{path=\"proxy\",decision=\"unavailable\"}")
            .await
            >= 1
    );
}

#[tokio::test]
async fn postgres_outage_refuses_config_edits_but_traffic_keeps_flowing() {
    let Some(redis) = common::RedisServer::start() else {
        return;
    };
    let Some(pg) = common::PostgresServer::start() else {
        return;
    };
    let o = Outage::start(&redis.url, Some(&pg.url)).await;
    let audit_failures_before = o.metric("tokenrein_audit_write_failures_total").await;

    drop(pg);

    let edit = o
        .http
        .put(format!("{}/admin/orgs/open/projects/bot", o.admin))
        .bearer_auth(ADMIN_TOKEN)
        .json(&json!({"budgets": {"run": {"limit_usd": 1.0, "mode": "enforce"}}}))
        .send()
        .await
        .unwrap();
    assert_eq!(edit.status(), StatusCode::SERVICE_UNAVAILABLE);
    let readyz = o
        .http
        .get(format!("{}/readyz", o.admin))
        .send()
        .await
        .unwrap();
    assert_eq!(readyz.status(), StatusCode::SERVICE_UNAVAILABLE);

    // Requests are budgeted in Redis as usual, for both fail modes.
    for key in [OPEN_KEY, CLOSED_KEY] {
        let resp = o.chat(key).await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(resp.headers().get("x-tr-degraded").is_none());
    }
    // Run controls still work; their audit entries cannot be written, and that is counted.
    let pause = o
        .http
        .post(format!("{}/admin/orgs/open/runs/outage-run/pause", o.admin))
        .bearer_auth(ADMIN_TOKEN)
        .send()
        .await
        .unwrap();
    assert_eq!(pause.status(), StatusCode::OK);
    assert!(o.metric("tokenrein_audit_write_failures_total").await > audit_failures_before);
}
