mod common;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::header::CONTENT_TYPE;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use serde_json::{Value, json};
use tokenrein::config::{Resolved, sha256_hex};
use tokenrein::ledger::Keys;
use tokenrein::pricing::Catalog;
use tokenrein::providers::{Provider, Usage};
use tokenrein::{AppState, admin, build_state, proxy};

const KEY: &str = "tl_test_bot_key_0001";
const SHADOW_KEY: &str = "tl_test_shadow_key_0001";
const ADMIN_TOKEN: &str = "admin-token-0123456789";

#[derive(Clone, Default)]
struct Upstream {
    seen: Arc<Mutex<Vec<(String, HeaderMap, Value)>>>,
}

impl Upstream {
    fn last(&self) -> (String, HeaderMap, Value) {
        self.seen
            .lock()
            .unwrap()
            .last()
            .cloned()
            .expect("upstream received a request")
    }
    fn count(&self) -> usize {
        self.seen.lock().unwrap().len()
    }
}

const OPENAI_USAGE: &str =
    r#"{"prompt_tokens":100,"completion_tokens":50,"prompt_tokens_details":{"cached_tokens":20}}"#;

async fn fake_openai(
    State(u): State<Upstream>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    u.seen
        .lock()
        .unwrap()
        .push(("openai".into(), headers, body.clone()));
    if body["stream"] == true {
        let mut sse = String::new();
        sse +=
            "data: {\"id\":\"c1\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Hel\"}}]}\n\n";
        sse +=
            "data: {\"id\":\"c1\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"lo\"}}]}\n\n";
        if body["stream_options"]["include_usage"] == true {
            sse += &format!("data: {{\"id\":\"c1\",\"choices\":[],\"usage\":{OPENAI_USAGE}}}\n\n");
        }
        sse += "data: [DONE]\n\n";
        return ([(CONTENT_TYPE, "text/event-stream")], sse).into_response();
    }
    let usage: Value = serde_json::from_str(OPENAI_USAGE).unwrap();
    Json(json!({
        "id": "c1",
        "object": "chat.completion",
        "choices": [{"index": 0, "message": {"role": "assistant", "content": "Hello"}}],
        "usage": usage
    }))
    .into_response()
}

async fn fake_anthropic(
    State(u): State<Upstream>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    u.seen
        .lock()
        .unwrap()
        .push(("anthropic".into(), headers, body.clone()));
    if body["stream"] == true {
        let sse = concat!(
            "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"usage\":{\"input_tokens\":200,\"output_tokens\":1}}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hi\"}}\n\n",
            "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":80}}\n\n",
            "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
        );
        return ([(CONTENT_TYPE, "text/event-stream")], sse).into_response();
    }
    Json(json!({
        "id": "msg_1",
        "type": "message",
        "content": [{"type": "text", "text": "Hi"}],
        "usage": {"input_tokens": 200, "output_tokens": 80}
    }))
    .into_response()
}

async fn serve(router: Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    format!("http://{addr}")
}

struct Harness {
    _redis: common::RedisServer,
    proxy: String,
    admin: String,
    state: Arc<AppState>,
    upstream: Upstream,
    http: reqwest::Client,
}

impl Harness {
    async fn start() -> Option<Self> {
        let redis = common::RedisServer::start()?;
        let upstream = Upstream::default();
        let upstream_url = serve(
            Router::new()
                .route("/v1/chat/completions", post(fake_openai))
                .route("/v1/messages", post(fake_anthropic))
                .with_state(upstream.clone()),
        )
        .await;
        let config = format!(
            r#"
            [server]
            listen = "127.0.0.1:0"
            admin_token_env = "ADMIN"
            upstream_timeout_secs = 30
            default_max_output_tokens = 1000
            [redis]
            url = "{redis}"
            [providers.openai]
            base_url = "{up}"
            api_key_env = "OPENAI_KEY"
            [providers.anthropic]
            base_url = "{up}"
            api_key_env = "ANTHROPIC_KEY"
            [[orgs]]
            id = "acme"
            anthropic_workspace_id = "wrkspc_test"
            [orgs.budgets]
            max_run_usd = 2.0
            [[orgs.projects]]
            id = "bot"
            virtual_key_hashes = ["{key}"]
            [[orgs.projects]]
            id = "shadow"
            virtual_key_hashes = ["{shadow}"]
            [orgs.projects.budgets]
            run = {{ limit_usd = 0.000001, mode = "check" }}
            "#,
            redis = redis.url,
            up = upstream_url,
            key = sha256_hex(KEY),
            shadow = sha256_hex(SHADOW_KEY),
        );
        let rt = Resolved::from_toml(&config, |name| match name {
            "OPENAI_KEY" => Some("sk-test-openai".into()),
            "ANTHROPIC_KEY" => Some("sk-ant-test".into()),
            "ADMIN" => Some(ADMIN_TOKEN.into()),
            _ => None,
        })
        .unwrap();
        let state = build_state(rt).await.unwrap();
        let proxy = serve(proxy::router(state.clone())).await;
        let admin = serve(admin::router(state.clone())).await;
        Some(Self {
            _redis: redis,
            proxy,
            admin,
            state,
            upstream,
            http: reqwest::Client::new(),
        })
    }

    fn chat(&self, run: &str, body: Value) -> reqwest::RequestBuilder {
        self.http
            .post(format!("{}/v1/chat/completions", self.proxy))
            .bearer_auth(KEY)
            .header("x-tr-run-id", run)
            .json(&body)
    }

    async fn admin(&self, run: &str, action: &str) -> reqwest::StatusCode {
        self.http
            .post(format!(
                "{}/admin/orgs/acme/runs/{run}/{action}",
                self.admin
            ))
            .bearer_auth(ADMIN_TOKEN)
            .send()
            .await
            .unwrap()
            .status()
    }

    async fn run_budget(&self, run: &str) -> (i64, i64) {
        let b = self
            .state
            .ledger
            .budget(&Keys::new("acme").run_budget(run))
            .await
            .unwrap();
        let get = |f: &str| b.get(f).and_then(|v| v.parse().ok()).unwrap_or(0);
        (get("spent"), get("reserved"))
    }

    /// Streams settle after the last byte is sent, so poll briefly.
    async fn settled_spend(&self, run: &str) -> i64 {
        for _ in 0..100 {
            let (spent, reserved) = self.run_budget(run).await;
            if reserved == 0 && spent > 0 {
                return spent;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("run {run} never settled: {:?}", self.run_budget(run).await);
    }
}

fn cost(provider: Provider, model: &str, usage: Usage) -> i64 {
    Catalog::builtin()
        .lookup(provider, model)
        .0
        .usage_cost(&usage) as i64
}

fn hi() -> Value {
    json!([{"role": "user", "content": "hi"}])
}

#[tokio::test]
async fn openai_request_is_forwarded_and_actual_cost_is_recorded() {
    let Some(h) = Harness::start().await else {
        return;
    };
    let resp = h
        .chat("run-a", json!({"model": "gpt-4o-mini", "messages": hi()}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.headers()["x-tr-run-id"], "run-a");
    assert!(resp.headers().contains_key("x-tr-request-id"));
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["choices"][0]["message"]["content"], "Hello");

    let (_, headers, sent) = h.upstream.last();
    assert_eq!(headers["authorization"], "Bearer sk-test-openai");
    assert_eq!(sent["max_completion_tokens"], 1000);

    let expected = cost(
        Provider::OpenAI,
        "gpt-4o-mini",
        Usage {
            input: 80,
            cached_input: 20,
            cache_write: 0,
            output: 50,
        },
    );
    assert_eq!(h.run_budget("run-a").await, (expected, 0));
}

#[tokio::test]
async fn openai_stream_passes_through_and_settles_from_usage_chunk() {
    let Some(h) = Harness::start().await else {
        return;
    };
    let resp = h
        .chat(
            "run-s",
            json!({"model": "gpt-4o-mini", "stream": true, "messages": hi()}),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let text = resp.text().await.unwrap();
    assert!(
        text.contains("\"Hel\"") && text.contains("[DONE]"),
        "{text}"
    );

    let (_, _, sent) = h.upstream.last();
    assert_eq!(sent["stream_options"]["include_usage"], true);
    let expected = cost(
        Provider::OpenAI,
        "gpt-4o-mini",
        Usage {
            input: 80,
            cached_input: 20,
            cache_write: 0,
            output: 50,
        },
    );
    assert_eq!(h.settled_spend("run-s").await, expected);
}

#[tokio::test]
async fn anthropic_stream_uses_provider_auth_and_settles() {
    let Some(h) = Harness::start().await else {
        return;
    };
    let resp = h
        .http
        .post(format!("{}/v1/messages", h.proxy))
        .header("x-api-key", KEY)
        .header("x-tr-run-id", "run-c")
        .json(&json!({"model": "claude-haiku-4-5", "max_tokens": 500, "stream": true, "messages": hi()}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert!(resp.text().await.unwrap().contains("message_stop"));

    let (_, headers, _) = h.upstream.last();
    assert_eq!(headers["x-api-key"], "sk-ant-test");
    assert_eq!(headers["anthropic-version"], "2023-06-01");
    let expected = cost(
        Provider::Anthropic,
        "claude-haiku-4-5",
        Usage {
            input: 200,
            output: 80,
            ..Usage::default()
        },
    );
    assert_eq!(h.settled_spend("run-c").await, expected);
}

#[tokio::test]
async fn over_budget_returns_402_in_each_provider_shape_without_calling_upstream() {
    let Some(h) = Harness::start().await else {
        return;
    };
    let resp = h
        .chat("run-b", json!({"model": "gpt-4o-mini", "messages": hi()}))
        .header("x-tr-run-budget-usd", "0.0001")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 402);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["type"], "tokenrein_budget_exceeded");

    let resp = h
        .http
        .post(format!("{}/v1/messages", h.proxy))
        .header("x-api-key", KEY)
        .header("x-tr-run-id", "run-b2")
        .header("x-tr-run-budget-usd", "0.0001")
        .json(&json!({"model": "claude-haiku-4-5", "max_tokens": 500, "messages": hi()}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 402);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(
        (body["type"].as_str(), body["error"]["type"].as_str()),
        (Some("error"), Some("tokenrein_budget_exceeded"))
    );
    assert_eq!(h.upstream.count(), 0);
}

#[tokio::test]
async fn child_spend_counts_against_the_parent_run_budget() {
    let Some(h) = Harness::start().await else {
        return;
    };
    let parent = h
        .chat("p", json!({"model": "gpt-4o-mini", "messages": hi()}))
        .header("x-tr-run-budget-usd", "0.001")
        .send()
        .await
        .unwrap();
    assert_eq!(parent.status(), 200);

    let child = |max_tokens: u64| {
        h.chat(
            "c",
            json!({"model": "gpt-4o-mini", "max_tokens": max_tokens, "messages": hi()}),
        )
        .header("x-tr-parent-run-id", "p")
    };
    assert_eq!(child(200).send().await.unwrap().status(), 200);
    let (parent_spent, _) = h.run_budget("p").await;
    let per_call = cost(
        Provider::OpenAI,
        "gpt-4o-mini",
        Usage {
            input: 80,
            cached_input: 20,
            cache_write: 0,
            output: 50,
        },
    );
    assert_eq!(parent_spent, 2 * per_call);

    let blocked = child(2000).send().await.unwrap();
    assert_eq!(blocked.status(), 402);
    let body: Value = blocked.json().await.unwrap();
    assert!(
        body["error"]["message"].as_str().unwrap().contains("run:p"),
        "{body}"
    );
}

#[tokio::test]
async fn shadow_mode_allows_and_warns() {
    let Some(h) = Harness::start().await else {
        return;
    };
    let resp = h
        .http
        .post(format!("{}/v1/chat/completions", h.proxy))
        .bearer_auth(SHADOW_KEY)
        .json(&json!({"model": "gpt-4o-mini", "messages": hi()}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let warning = resp.headers()["x-tr-warning"].to_str().unwrap().to_string();
    assert!(warning.contains("run:"), "{warning}");
}

#[tokio::test]
async fn pause_resume_and_kill_apply_to_the_run_and_its_children() {
    let Some(h) = Harness::start().await else {
        return;
    };
    let child_call = || {
        h.chat("child-1", json!({"model": "gpt-4o-mini", "messages": hi()}))
            .header("x-tr-parent-run-id", "r9")
    };
    let call = || h.chat("r9", json!({"model": "gpt-4o-mini", "messages": hi()}));

    assert_eq!(h.admin("r9", "pause").await, 200);
    let resp = call().send().await.unwrap();
    assert_eq!(resp.status(), 403);
    assert_eq!(
        resp.json::<Value>().await.unwrap()["error"]["type"],
        "tokenrein_run_paused"
    );
    assert_eq!(child_call().send().await.unwrap().status(), 403);

    assert_eq!(h.admin("r9", "resume").await, 200);
    assert_eq!(call().send().await.unwrap().status(), 200);
    assert_eq!(child_call().send().await.unwrap().status(), 200);

    assert_eq!(h.admin("r9", "kill").await, 200);
    let resp = call().send().await.unwrap();
    assert_eq!(resp.status(), 403);
    assert_eq!(
        resp.json::<Value>().await.unwrap()["error"]["type"],
        "tokenrein_run_killed"
    );
    assert_eq!(h.admin("r9", "resume").await, 409);

    let unauthorized = h
        .http
        .post(format!("{}/admin/orgs/acme/runs/r9/resume", h.admin))
        .send()
        .await
        .unwrap();
    assert_eq!(unauthorized.status(), 401);
}

#[tokio::test]
async fn rejects_bad_keys_and_ids() {
    let Some(h) = Harness::start().await else {
        return;
    };
    let resp = h
        .http
        .post(format!("{}/v1/chat/completions", h.proxy))
        .bearer_auth("tl_wrong")
        .json(&json!({"model": "gpt-4o-mini", "messages": hi()}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);
    let resp = h
        .chat("bad:id", json!({"model": "gpt-4o-mini", "messages": hi()}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
    assert_eq!(h.upstream.count(), 0);
}

#[tokio::test]
async fn anthropic_calls_carry_the_org_workspace_unless_the_client_sends_one() {
    let Some(h) = Harness::start().await else {
        return;
    };
    let send = |workspace: Option<&'static str>| {
        let mut req = h
            .http
            .post(format!("{}/v1/messages", h.proxy))
            .header("x-api-key", KEY)
            .header("x-tr-run-id", "run-ws");
        if let Some(ws) = workspace {
            req = req.header("anthropic-workspace-id", ws);
        }
        req.json(&json!({"model": "claude-haiku-4-5", "max_tokens": 50, "messages": hi()}))
            .send()
    };
    assert_eq!(send(None).await.unwrap().status(), 200);
    let (_, headers, _) = h.upstream.last();
    assert_eq!(headers["anthropic-workspace-id"], "wrkspc_test");

    assert_eq!(send(Some("wrkspc_client")).await.unwrap().status(), 200);
    let (_, headers, _) = h.upstream.last();
    assert_eq!(headers["anthropic-workspace-id"], "wrkspc_client");

    // Not added to OpenAI calls.
    let resp = h
        .chat(
            "run-ws-oai",
            json!({"model": "gpt-4o-mini", "messages": hi()}),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let (provider, headers, _) = h.upstream.last();
    assert_eq!(provider, "openai");
    assert!(headers.get("anthropic-workspace-id").is_none());
}

#[tokio::test]
async fn headers_named_before_the_rename_still_work() {
    let Some(h) = Harness::start().await else {
        return;
    };
    // x-tl-* (pre-rename names) are read as x-tr-*: here a run id and a budget that is too small.
    let resp = h
        .http
        .post(format!("{}/v1/chat/completions", h.proxy))
        .bearer_auth(KEY)
        .header("x-tl-run-id", "old-run")
        .header("x-tl-run-budget-usd", "0.0001")
        .json(&json!({"model": "gpt-4o-mini", "messages": hi()}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 402);
    assert_eq!(resp.headers()["x-tr-run-id"], "old-run");
    // When both are sent, the new name wins.
    let resp = h
        .chat("new-run", json!({"model": "gpt-4o-mini", "messages": hi()}))
        .header("x-tl-run-id", "old-run-2")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.headers()["x-tr-run-id"], "new-run");
}
