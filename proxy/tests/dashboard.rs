//! Managed orgs / keys / budgets and dashboard activity, against a real redis-server.

mod common;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use reqwest::{Method, StatusCode};
use serde_json::{Value, json};
use tokenlens_proxy::config::{Resolved, sha256_hex};
use tokenlens_proxy::{AppState, admin, build_state, control, proxy};

const CONFIG_KEY: &str = "tl_config_key_0001";
const ADMIN_TOKEN: &str = "admin-token-0123456789";
const SECRETS_KEY: &str = "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f";

/// The Authorization header of the last request the fake provider received.
type SeenAuth = Arc<Mutex<Option<String>>>;

async fn fake_openai(State(seen): State<SeenAuth>, headers: HeaderMap) -> Json<Value> {
    *seen.lock().unwrap() = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    Json(json!({
        "id": "c1",
        "object": "chat.completion",
        "choices": [{"index": 0, "message": {"role": "assistant", "content": "Hello"}}],
        "usage": {"prompt_tokens": 100, "completion_tokens": 50}
    }))
}

/// Accepts only the environment key, like a provider would.
async fn fake_models(headers: HeaderMap) -> axum::response::Response {
    let auth = headers.get("authorization").and_then(|v| v.to_str().ok());
    if auth == Some("Bearer sk-test") {
        Json(json!({"data": [{"id": "gpt-4o-mini"}]})).into_response()
    } else {
        StatusCode::UNAUTHORIZED.into_response()
    }
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
        secrets_key_env = "SECRETS"
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
    instance_with(redis, upstream, true).await
}

async fn instance_with(redis: &str, upstream: &str, secrets: bool) -> Arc<AppState> {
    let rt = Resolved::from_toml(&config(redis, upstream), move |name| match name {
        "OPENAI_KEY" => Some("sk-test".into()),
        "ADMIN" => Some(ADMIN_TOKEN.into()),
        "SECRETS" if secrets => Some(SECRETS_KEY.into()),
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
    upstream_auth: SeenAuth,
    state: Arc<AppState>,
    proxy: String,
    admin: String,
    http: reqwest::Client,
}

impl H {
    async fn start() -> Option<Self> {
        Self::start_with(true).await
    }

    async fn start_with(secrets: bool) -> Option<Self> {
        let redis = common::RedisServer::start()?;
        let upstream_auth = SeenAuth::default();
        let upstream = serve(
            Router::new()
                .route("/v1/chat/completions", post(fake_openai))
                .route("/v1/models", get(fake_models))
                .with_state(upstream_auth.clone()),
        )
        .await;
        let state = instance_with(&redis.url, &upstream, secrets).await;
        let proxy = serve(proxy::router(state.clone())).await;
        let admin = serve(admin::router(state.clone())).await;
        Some(Self {
            redis_url: redis.url.clone(),
            _redis: redis,
            upstream,
            upstream_auth,
            state,
            proxy,
            admin,
            http: reqwest::Client::new(),
        })
    }

    async fn chat(&self, key: &str, run: &str, parent: Option<&str>) -> StatusCode {
        self.chat_as(key, run, parent, None).await
    }

    async fn chat_as(
        &self,
        key: &str,
        run: &str,
        parent: Option<&str>,
        user: Option<&str>,
    ) -> StatusCode {
        let mut req = self
            .http
            .post(format!("{}/v1/chat/completions", self.proxy))
            .bearer_auth(key)
            .header("x-tl-run-id", run)
            .header("x-tl-agent", "planner");
        if let Some(p) = parent {
            req = req.header("x-tl-parent-run-id", p);
        }
        if let Some(u) = user {
            req = req.header("x-tl-user", u);
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

    async fn put_provider_key(&self, provider: &str, key: &str) -> StatusCode {
        let path = format!("/admin/orgs/acme/provider-keys/{provider}");
        self.admin(Method::PUT, &path, Some(json!({ "api_key": key })))
            .await
            .0
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

#[tokio::test]
async fn provider_keys_saved_in_the_dashboard_are_used_and_never_returned() {
    let Some(h) = H::start().await else { return };
    let ov = h.get("/admin/overview").await;
    assert_eq!(ov["secrets_enabled"], true);
    assert_eq!(
        ov["orgs"][0]["providers"]["openai"]["source"],
        "environment"
    );
    assert_eq!(ov["orgs"][0]["providers"]["anthropic"]["configured"], false);

    const REAL: &str = "sk-dashboard-key-123456";
    let (s, body) = h
        .admin(
            Method::PUT,
            "/admin/orgs/acme/provider-keys/openai",
            Some(json!({"api_key": REAL})),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{body}");
    assert_eq!(body["last4"], "3456");
    assert_eq!(h.chat(CONFIG_KEY, "pk", None).await, StatusCode::OK);
    assert_eq!(
        h.upstream_auth.lock().unwrap().as_deref(),
        Some("Bearer sk-dashboard-key-123456")
    );

    let ov = h.get("/admin/overview").await;
    let st = &ov["orgs"][0]["providers"]["openai"];
    assert_eq!(st["source"], "dashboard");
    assert_eq!(st["last4"], "3456");
    assert!(!ov.to_string().contains(REAL));
    // Stored encrypted.
    let (_, doc) = h.state.ledger.control_load().await.unwrap().unwrap();
    assert!(doc.contains("provider_secrets"));
    assert!(!doc.contains(REAL));
    // Another instance with the same secrets key uses it too.
    let other = instance(&h.redis_url, &h.upstream).await;
    assert_eq!(
        other
            .rt()
            .provider_key(0, tokenlens_proxy::providers::Provider::OpenAI),
        Some(REAL)
    );

    // Removing it falls back to the environment key.
    let (s, _) = h
        .admin(
            Method::DELETE,
            "/admin/orgs/acme/provider-keys/openai",
            None,
        )
        .await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    assert_eq!(h.chat(CONFIG_KEY, "pk", None).await, StatusCode::OK);
    assert_eq!(
        h.upstream_auth.lock().unwrap().as_deref(),
        Some("Bearer sk-test")
    );
}

#[tokio::test]
async fn provider_key_input_is_validated() {
    let Some(h) = H::start().await else { return };
    let cases = [
        ("openai", "sk-abc def-123", StatusCode::BAD_REQUEST),
        ("openai", "short", StatusCode::BAD_REQUEST),
        ("gemini", "sk-abcdefgh", StatusCode::NOT_FOUND),
        // No [providers.anthropic] section in this config.
        ("anthropic", "sk-ant-abcdefgh", StatusCode::CONFLICT),
    ];
    for (provider, key, expected) in cases {
        let path = format!("/admin/orgs/acme/provider-keys/{provider}");
        let (s, body) = h
            .admin(Method::PUT, &path, Some(json!({ "api_key": key })))
            .await;
        assert_eq!(s, expected, "{provider} {key:?}: {body}");
    }
}

#[tokio::test]
async fn provider_keys_need_a_secrets_key() {
    let Some(h) = H::start_with(false).await else {
        return;
    };
    assert_eq!(h.get("/admin/overview").await["secrets_enabled"], false);
    let (s, body) = h
        .admin(
            Method::PUT,
            "/admin/orgs/acme/provider-keys/openai",
            Some(json!({"api_key": "sk-abcdefgh"})),
        )
        .await;
    assert_eq!(s, StatusCode::CONFLICT);
    assert!(body["error"].as_str().unwrap().contains("SECRETS"));
}

#[tokio::test]
async fn packs_start_in_check_and_the_review_shows_what_enforce_would_block() {
    let Some(h) = H::start().await else { return };
    let packs = h.get("/admin/packs").await;
    assert_eq!(packs["packs"].as_array().unwrap().len(), 3);

    let (s, body) = h
        .admin(
            Method::POST,
            "/admin/orgs/acme/projects/bot/pack",
            Some(json!({"pack": "production-agents"})),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{body}");
    let ov = h.get("/admin/overview").await;
    let bot = &ov["orgs"][0]["projects"][0];
    assert_eq!(bot["pack"], "production-agents");
    assert_eq!(bot["budgets"]["run"]["mode"], "check");

    // A tiny run budget in Check mode: allowed, but recorded as "would have been blocked".
    let tiny = json!({"budgets": {"run": {"limit_usd": 0.000001, "mode": "check"}}});
    let (s, _) = h
        .admin(Method::PUT, "/admin/orgs/acme/projects/bot", Some(tiny))
        .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(h.chat(CONFIG_KEY, "shadow-run", None).await, StatusCode::OK);
    assert_eq!(h.chat(CONFIG_KEY, "shadow-run", None).await, StatusCode::OK);
    // Exceeds its own run limit and its parent's: still one request.
    assert_eq!(
        h.chat(CONFIG_KEY, "shadow-child", Some("shadow-run")).await,
        StatusCode::OK
    );

    let review = h
        .eventually("/admin/orgs/acme/review", |b| {
            b["items"]
                .as_array()
                .unwrap()
                .iter()
                .any(|i| i["policy"] == "run" && i["requests"] == 3)
        })
        .await;
    let item = review["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["policy"] == "run")
        .unwrap()
        .clone();
    assert_eq!(item["project"], "bot");
    assert_eq!(review["check_requests"], 3);
    assert_eq!(item["run_count"], 2);
    assert_eq!(item["current"]["mode"], "check");
    assert!(item["cost_micros"].as_i64().unwrap() > 0);

    // One click later: Enforce blocks a new run.
    let enforce = json!({"budgets": {"run": {"limit_usd": 0.000001, "mode": "enforce"}}});
    let (s, _) = h
        .admin(Method::PUT, "/admin/orgs/acme/projects/bot", Some(enforce))
        .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(
        h.chat(CONFIG_KEY, "enforced-run", None).await,
        StatusCode::PAYMENT_REQUIRED
    );
}

#[tokio::test]
async fn run_timeline_includes_sub_agent_runs() {
    let Some(h) = H::start().await else { return };
    assert_eq!(h.chat(CONFIG_KEY, "t-root", None).await, StatusCode::OK);
    assert_eq!(
        h.chat(CONFIG_KEY, "t-child", Some("t-root")).await,
        StatusCode::OK
    );
    assert_eq!(
        h.chat(CONFIG_KEY, "t-grandchild", Some("t-child")).await,
        StatusCode::OK
    );
    assert_eq!(h.chat(CONFIG_KEY, "t-root", None).await, StatusCode::OK);
    let tl = h
        .eventually("/admin/orgs/acme/runs/t-root/timeline", |b| {
            b["events"].as_array().unwrap().len() == 4
        })
        .await;
    let runs: Vec<&str> = tl["runs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["run_id"].as_str().unwrap())
        .collect();
    assert_eq!(runs, ["t-root", "t-child", "t-grandchild"]);
    let order: Vec<&str> = tl["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["run_id"].as_str().unwrap())
        .collect();
    assert_eq!(order, ["t-root", "t-child", "t-grandchild", "t-root"]);
    // A child's own timeline does not include its parent.
    let child = h.get("/admin/orgs/acme/runs/t-child/timeline").await;
    assert_eq!(child["events"].as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn spend_is_broken_down_by_model_and_user() {
    let Some(h) = H::start().await else { return };
    assert_eq!(
        h.chat_as(CONFIG_KEY, "s1", None, Some("alice")).await,
        StatusCode::OK
    );
    assert_eq!(
        h.chat_as(CONFIG_KEY, "s2", None, Some("alice")).await,
        StatusCode::OK
    );
    assert_eq!(
        h.chat_as(CONFIG_KEY, "s3", None, Some("bob")).await,
        StatusCode::OK
    );
    assert_eq!(
        h.chat_as(CONFIG_KEY, "s4", None, Some("not valid")).await,
        StatusCode::BAD_REQUEST
    );
    let spend = h
        .eventually("/admin/orgs/acme/spend?days=1", |b| {
            b["days"][0]["requests"] == 3
        })
        .await;
    assert!(spend["days"][0]["cost_micros"].as_i64().unwrap() > 0);
    let users = spend["by"]["user"].as_array().unwrap();
    assert_eq!(users[0]["key"], "alice");
    assert_eq!(users[0]["requests"], 2);
    assert_eq!(users[1]["key"], "bob");
    let models = spend["by"]["model"].as_array().unwrap();
    assert_eq!(models[0]["key"], "openai/gpt-4o-mini");
    assert_eq!(models[0]["requests"], 3);
    assert_eq!(spend["by"]["agent"][0]["key"], "bot/planner");
}

#[tokio::test]
async fn provider_keys_can_be_tested() {
    let Some(h) = H::start().await else { return };
    let (s, body) = h
        .admin(
            Method::POST,
            "/admin/orgs/acme/provider-keys/openai/test",
            None,
        )
        .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(body["ok"], true, "{body}");
    assert_eq!(
        h.put_provider_key("openai", "sk-wrong-key-123").await,
        StatusCode::OK
    );
    let (_, body) = h
        .admin(
            Method::POST,
            "/admin/orgs/acme/provider-keys/openai/test",
            None,
        )
        .await;
    assert_eq!(body["ok"], false);
    assert_eq!(body["status"], 401);
    assert!(!body.to_string().contains("sk-wrong-key-123"));
}

#[tokio::test]
async fn anthropic_workspace_id_is_set_cleared_and_validated() {
    let Some(h) = H::start().await else { return };
    let set = |ws: &str| json!({ "anthropic_workspace_id": ws });
    let (s, _) = h
        .admin(Method::PUT, "/admin/orgs/acme", Some(set("wrkspc_01abc")))
        .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(
        h.get("/admin/overview").await["orgs"][0]["anthropic_workspace_id"],
        "wrkspc_01abc"
    );
    let (s, _) = h
        .admin(Method::PUT, "/admin/orgs/acme", Some(set("not valid")))
        .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let (s, _) = h
        .admin(Method::PUT, "/admin/orgs/acme", Some(set("  ")))
        .await;
    assert_eq!(s, StatusCode::OK);
    assert!(h.get("/admin/overview").await["orgs"][0]["anthropic_workspace_id"].is_null());
}
