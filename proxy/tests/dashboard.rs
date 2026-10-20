//! Managed orgs / keys / budgets and dashboard activity, against a real redis-server.

mod common;

use std::sync::Arc;
use std::time::Duration;

use axum::routing::post;
use axum::{Json, Router};
use reqwest::{Method, StatusCode};
use serde_json::{Value, json};
use tokenlens_proxy::config::{Resolved, sha256_hex};
use tokenlens_proxy::{AppState, admin, build_state, control, proxy};

const CONFIG_KEY: &str = "tl_config_key_0001";
const ADMIN_TOKEN: &str = "admin-token-0123456789";

async fn fake_openai() -> Json<Value> {
    Json(json!({
        "id": "c1",
        "object": "chat.completion",
        "choices": [{"index": 0, "message": {"role": "assistant", "content": "Hello"}}],
        "usage": {"prompt_tokens": 100, "completion_tokens": 50}
    }))
}

async fn serve(router: Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    format!("http://{addr}")
}

fn config(redis: &str, upstream: &str) -> String {
    format!(
        r#"
        [server]
        listen = "127.0.0.1:0"
        admin_token_env = "ADMIN"
        default_max_output_tokens = 1000
        [redis]
        url = "{redis}"
        [providers.openai]
        base_url = "{upstream}"
        api_key_env = "OPENAI_KEY"
        [[orgs]]
        id = "acme"
        [orgs.budgets]
        org_monthly = {{ limit_usd = 100.0, mode = "enforce" }}
        [[orgs.projects]]
        id = "bot"
        virtual_key_hashes = ["{key}"]
        "#,
        key = sha256_hex(CONFIG_KEY),
    )
}

async fn instance(redis: &str, upstream: &str) -> Arc<AppState> {
    let rt = Resolved::from_toml(&config(redis, upstream), |name| match name {
        "OPENAI_KEY" => Some("sk-test".into()),
        "ADMIN" => Some(ADMIN_TOKEN.into()),
        _ => None,
    })
    .unwrap();
    let state = build_state(rt).await.unwrap();
    control::init(&state).await.unwrap();
    state
}

struct H {
    _redis: common::RedisServer,
    redis_url: String,
    upstream: String,
    state: Arc<AppState>,
    proxy: String,
    admin: String,
    http: reqwest::Client,
}

impl H {
    async fn start() -> Option<Self> {
        let redis = common::RedisServer::start()?;
        let upstream = serve(Router::new().route("/v1/chat/completions", post(fake_openai))).await;
        let state = instance(&redis.url, &upstream).await;
        let proxy = serve(proxy::router(state.clone())).await;
        let admin = serve(admin::router(state.clone())).await;
        Some(Self {
            redis_url: redis.url.clone(),
            _redis: redis,
            upstream,
            state,
            proxy,
            admin,
            http: reqwest::Client::new(),
        })
    }

    async fn chat(&self, key: &str, run: &str, parent: Option<&str>) -> StatusCode {
        let mut req = self
            .http
            .post(format!("{}/v1/chat/completions", self.proxy))
            .bearer_auth(key)
            .header("x-tl-run-id", run)
            .header("x-tl-agent", "planner");
        if let Some(p) = parent {
            req = req.header("x-tl-parent-run-id", p);
        }
        req.json(&json!({"model": "gpt-4o-mini", "messages": [{"role": "user", "content": "hi"}]}))
            .send()
            .await
            .unwrap()
            .status()
    }

    async fn admin(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        let mut req = self
            .http
            .request(method, format!("{}{path}", self.admin))
            .bearer_auth(ADMIN_TOKEN);
        if let Some(b) = body {
            req = req.json(&b);
        }
        let resp = req.send().await.unwrap();
        let status = resp.status();
        let body = resp.json().await.unwrap_or(Value::Null);
        (status, body)
    }

    async fn get(&self, path: &str) -> Value {
        let (status, body) = self.admin(reqwest::Method::GET, path, None).await;
        assert_eq!(status, StatusCode::OK, "{path}: {body}");
        body
    }

    /// Activity is written in the background; wait until `check` passes.
    async fn eventually(&self, path: &str, check: impl Fn(&Value) -> bool) -> Value {
        for _ in 0..100 {
            let body = self.get(path).await;
            if check(&body) {
                return body;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("{path} never matched: {}", self.get(path).await);
    }
}

#[tokio::test]
async fn seeds_from_config_and_shows_overview() {
    let Some(h) = H::start().await else { return };
    assert_eq!(h.chat(CONFIG_KEY, "r1", None).await, StatusCode::OK);
    let ov = h.get("/admin/overview").await;
    assert_eq!(ov["managed"], true);
    let org = &ov["orgs"][0];
    assert_eq!(org["id"], "acme");
    assert_eq!(org["budgets"]["org_monthly"]["limit_usd"], 100.0);
    assert_eq!(org["projects"][0]["id"], "bot");
    assert_eq!(org["projects"][0]["keys"][0]["source"], "config");
    // 100 input + 50 output tokens of gpt-4o-mini.
    assert!(
        org["spend"]["org_monthly"]["spent_micros"]
            .as_i64()
            .unwrap()
            > 0
    );
}

#[tokio::test]
async fn keys_created_and_revoked_from_the_dashboard_apply_immediately() {
    let Some(h) = H::start().await else { return };
    let (s, _) = h
        .admin(
            Method::POST,
            "/admin/orgs/acme/projects",
            Some(json!({"id": "research"})),
        )
        .await;
    assert_eq!(s, StatusCode::CREATED);
    let (s, created) = h
        .admin(
            Method::POST,
            "/admin/orgs/acme/projects/research/keys",
            Some(json!({"name": "research prod"})),
        )
        .await;
    assert_eq!(s, StatusCode::CREATED);
    let key = created["key"].as_str().unwrap().to_string();
    assert!(key.starts_with("tl_"));
    assert!(key.starts_with(created["prefix"].as_str().unwrap()));

    assert_eq!(h.chat(&key, "rk", None).await, StatusCode::OK);

    // The key itself is never returned again; only its prefix.
    let ov = h.get("/admin/overview").await;
    let shown = ov["orgs"][0]["projects"][1]["keys"][0].clone();
    assert_eq!(shown["name"], "research prod");
    assert!(shown.get("key").is_none());
    assert!(!ov.to_string().contains(&key));

    let id = created["id"].as_str().unwrap();
    let (s, _) = h
        .admin(
            Method::DELETE,
            &format!("/admin/orgs/acme/projects/research/keys/{id}"),
            None,
        )
        .await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    assert_eq!(h.chat(&key, "rk", None).await, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn limits_set_from_the_dashboard_are_enforced() {
    let Some(h) = H::start().await else { return };
    let (s, _) = h
        .admin(
            Method::PUT,
            "/admin/orgs/acme/projects/bot",
            Some(json!({"budgets": {"agent_daily": {"limit_usd": 0.000001, "mode": "enforce"}}})),
        )
        .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(
        h.chat(CONFIG_KEY, "r2", None).await,
        StatusCode::PAYMENT_REQUIRED
    );

    // Back to no agent limit.
    let (s, _) = h
        .admin(
            Method::PUT,
            "/admin/orgs/acme/projects/bot",
            Some(json!({"budgets": {}})),
        )
        .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(h.chat(CONFIG_KEY, "r2", None).await, StatusCode::OK);
}

#[tokio::test]
async fn invalid_edits_are_rejected() {
    let Some(h) = H::start().await else { return };
    let negative = json!({"budgets": {"org_monthly": {"limit_usd": -1.0, "mode": "enforce"}}});
    let (s, body) = h
        .admin(Method::PUT, "/admin/orgs/acme", Some(negative))
        .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{body}");
    let (s, _) = h
        .admin(
            Method::POST,
            "/admin/orgs/acme/projects",
            Some(json!({"id": "bad id"})),
        )
        .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let (s, _) = h
        .admin(
            Method::POST,
            "/admin/orgs/acme/projects",
            Some(json!({"id": "bot"})),
        )
        .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let (s, _) = h
        .admin(
            Method::POST,
            "/admin/orgs/acme/projects/nope/keys",
            Some(json!({"name": "x"})),
        )
        .await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let unauthenticated = h
        .http
        .get(format!("{}/admin/overview", h.admin))
        .send()
        .await
        .unwrap();
    assert_eq!(unauthenticated.status(), StatusCode::UNAUTHORIZED);
    // A stale writer loses instead of overwriting.
    assert_eq!(h.state.ledger.control_store(0, "[]").await.unwrap(), None);
}

#[tokio::test]
async fn other_instances_pick_up_changes() {
    let Some(h) = H::start().await else { return };
    let other = instance(&h.redis_url, &h.upstream).await;
    let (s, created) = h
        .admin(
            Method::POST,
            "/admin/orgs/acme/projects/bot/keys",
            Some(json!({"name": "k"})),
        )
        .await;
    assert_eq!(s, StatusCode::CREATED);
    let hash = sha256_hex(created["key"].as_str().unwrap());
    assert!(!other.rt().keys.contains_key(&hash));
    control::sync(&other).await.unwrap();
    assert!(other.rt().keys.contains_key(&hash));

    // A restart keeps dashboard changes instead of re-reading the config file's orgs.
    let restarted = instance(&h.redis_url, &h.upstream).await;
    assert!(restarted.rt().keys.contains_key(&hash));
}

#[tokio::test]
async fn runs_agents_and_events_are_recorded() {
    let Some(h) = H::start().await else { return };
    assert_eq!(h.chat(CONFIG_KEY, "parent-run", None).await, StatusCode::OK);
    assert_eq!(
        h.chat(CONFIG_KEY, "child-run", Some("parent-run")).await,
        StatusCode::OK
    );
    let (s, _) = h
        .admin(Method::POST, "/admin/orgs/acme/runs/parent-run/pause", None)
        .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(
        h.chat(CONFIG_KEY, "child-run", Some("parent-run")).await,
        StatusCode::FORBIDDEN
    );

    let runs = h
        .eventually("/admin/orgs/acme/runs", |b| {
            b["runs"]
                .as_array()
                .unwrap()
                .iter()
                .any(|r| r["run_id"] == "child-run" && r["requests"] == 2)
        })
        .await;
    let runs = runs["runs"].as_array().unwrap();
    let child = runs.iter().find(|r| r["run_id"] == "child-run").unwrap();
    assert_eq!(child["parent_run_id"], "parent-run");
    assert_eq!(child["project"], "bot");
    assert_eq!(child["agent"], "planner");
    assert_eq!(child["blocked"], 1);
    let parent = runs.iter().find(|r| r["run_id"] == "parent-run").unwrap();
    assert_eq!(parent["state"], "paused");
    // The parent's spend includes the child's.
    assert!(
        parent["budget"]["spent_micros"].as_i64().unwrap()
            > child["budget"]["spent_micros"].as_i64().unwrap()
    );

    let agents = h.get("/admin/orgs/acme/agents").await;
    assert_eq!(agents["agents"][0]["project"], "bot");
    assert_eq!(agents["agents"][0]["agent"], "planner");
    assert!(
        agents["agents"][0]["today"]["spent_micros"]
            .as_i64()
            .unwrap()
            > 0
    );

    let events = h.get("/admin/orgs/acme/events?limit=10").await;
    let events = events["events"].as_array().unwrap();
    assert_eq!(events.len(), 3);
    assert_eq!(events[0]["decision"], "blocked_run_stopped");
}

#[tokio::test]
async fn serves_the_dashboard_page() {
    let Some(h) = H::start().await else { return };
    let resp = h.http.get(format!("{}/", h.admin)).send().await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let csp = resp.headers()["content-security-policy"]
        .to_str()
        .unwrap()
        .to_string();
    assert!(csp.contains("default-src 'none'"));
    assert!(resp.text().await.unwrap().contains("TokenLens Dashboard"));
}
