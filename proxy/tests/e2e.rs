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
const TUNED_KEY: &str = "tl_test_tuned_key_0001";
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
            [[orgs.projects]]
            id = "tuned"
            virtual_key_hashes = ["{tuned}"]
            optimize = {{ cache = true, routes = [{{ from = "claude-opus-5-5", to = "claude-haiku-5-5" }}] }}
            [orgs.projects.budgets]
            run = {{ limit_usd = 1.0, mode = "enforce" }}
            loops = {{ action = "pause", max_repeats = 3, nudge = true, max_calls_without_progress = 4 }}
            "#,
            redis = redis.url,
            up = upstream_url,
            key = sha256_hex(KEY),
            shadow = sha256_hex(SHADOW_KEY),
            tuned = sha256_hex(TUNED_KEY),
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
            ..Usage::default()
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
            ..Usage::default()
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
            ..Usage::default()
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

#[tokio::test]
async fn proxied_requests_record_their_shape_and_tool_results() {
    let Some(h) = Harness::start().await else {
        return;
    };
    let send = |body: Value| {
        h.http
            .post(format!("{}/v1/messages", h.proxy))
            .header("x-api-key", KEY)
            .header("x-tr-run-id", "shape-1")
            .json(&body)
            .send()
    };
    let bash =
        json!({"name": "Bash", "description": "Run a command", "input_schema": {"type": "object"}});
    let read =
        json!({"name": "Read", "description": "Read a file", "input_schema": {"type": "object"}});
    let first = json!({"model": "claude-haiku-4-5", "max_tokens": 50, "system": "Be careful.",
        "tools": [bash.clone()], "messages": hi()});
    assert_eq!(send(first).await.unwrap().status(), 200);
    // The next turn answers a failed Bash call, and the tool list grew.
    let second = json!({"model": "claude-haiku-4-5", "max_tokens": 50, "system": "Be careful.",
    "tools": [bash, read],
    "messages": [
        {"role": "user", "content": "hi"},
        {"role": "assistant", "content": [{"type": "tool_use", "id": "t1", "name": "Bash", "input": {"cmd": "ls"}}]},
        {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "t1", "is_error": true, "content": "denied"}]},
    ]});
    assert_eq!(send(second).await.unwrap().status(), 200);

    let mut timeline = Value::Null;
    for _ in 0..100 {
        timeline = h
            .http
            .get(format!("{}/admin/orgs/acme/runs/shape-1/timeline", h.admin))
            .bearer_auth(ADMIN_TOKEN)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let events = timeline["events"].as_array().map_or(0, Vec::len);
        let steps = timeline["steps"].as_array().map_or(0, Vec::len);
        if events == 2 && steps == 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let events = timeline["events"].as_array().unwrap();
    let prompts: Vec<&Value> = events.iter().map(|e| &e["prompt"]).collect();
    // Oldest first or newest first, one of them saw the tool list change.
    assert!(
        prompts
            .iter()
            .any(|p| p["change"] == "tools" && p["tool_defs"] == 2),
        "{prompts:?}"
    );
    assert!(
        prompts
            .iter()
            .any(|p| p["change"].is_null() && p["tool_defs"] == 1),
        "{prompts:?}"
    );
    assert!(prompts.iter().all(|p| p["system_tokens"] == 3));
    let steps = timeline["steps"].as_array().unwrap();
    let kinds: Vec<(&str, &str, &str)> = steps
        .iter()
        .map(|s| {
            (
                s["kind"].as_str().unwrap(),
                s["name"].as_str().unwrap(),
                s["source"].as_str().unwrap(),
            )
        })
        .collect();
    assert!(kinds.contains(&("tool_call", "Bash", "proxy")), "{kinds:?}");
    let result = steps.iter().find(|s| s["kind"] == "tool_result").unwrap();
    assert_eq!(result["status"], "error");
    assert_eq!(result["size"], 6);
    // Tool names are kept per agent for the unused-tools check.
    let sets = tokenrein::prompts::tool_usage(&h.state, "acme", 1).await;
    let (defined, called) = sets.values().next().unwrap();
    assert_eq!(
        defined.iter().map(String::as_str).collect::<Vec<_>>(),
        ["Bash", "Read"]
    );
    assert_eq!(
        called.iter().map(String::as_str).collect::<Vec<_>>(),
        ["Bash"]
    );
}

fn tuned(h: &Harness, run: &str, body: Value) -> reqwest::RequestBuilder {
    h.http
        .post(format!("{}/v1/messages", h.proxy))
        .header("x-api-key", TUNED_KEY)
        .header("x-tr-run-id", run)
        .json(&body)
}

/// About 5,000 tokens of conversation: enough to be worth caching.
fn long_history() -> Value {
    json!([{"role": "user", "content": "context ".repeat(2_500)}])
}

#[tokio::test]
async fn the_proxy_adds_forgotten_caching_and_routes_simple_steps() {
    let Some(h) = Harness::start().await else {
        return;
    };
    // A long conversation without cache_control gets automatic caching turned on.
    let resp = tuned(&h, "opt-1", json!({"model": "claude-opus-5-5", "max_tokens": 2000,
        "tools": [{"name": "Bash", "input_schema": {"type": "object"}}], "messages": long_history()}))
    .send()
    .await
    .unwrap();
    assert_eq!(resp.status(), 200);
    let (_, _, body) = h.upstream.last();
    assert_eq!(body["cache_control"], json!({"type": "ephemeral"}));
    assert_eq!(body["model"], "claude-opus-5-5");
    // A short step with no tools goes to the cheaper model.
    let resp = tuned(
        &h,
        "opt-1",
        json!({"model": "claude-opus-5-5", "max_tokens": 200,
        "messages": [{"role": "user", "content": "Title for this chat?"}]}),
    )
    .send()
    .await
    .unwrap();
    assert_eq!(resp.status(), 200);
    let (_, _, body) = h.upstream.last();
    assert_eq!(body["model"], "claude-haiku-5-5");
    assert!(body.get("cache_control").is_none());
    // Other projects are left alone.
    let resp = h
        .http
        .post(format!("{}/v1/messages", h.proxy))
        .header("x-api-key", KEY)
        .header("x-tr-run-id", "opt-2")
        .json(&json!({"model": "claude-opus-5-5", "max_tokens": 200, "messages": long_history()}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let (_, _, body) = h.upstream.last();
    assert!(body.get("cache_control").is_none());
    assert_eq!(body["model"], "claude-opus-5-5");
}

#[tokio::test]
async fn a_looping_run_is_nudged_once_then_paused() {
    let Some(h) = Harness::start().await else {
        return;
    };
    let same = json!({"model": "claude-haiku-4-5", "max_tokens": 50, "messages": hi()});
    // Two identical calls go through untouched.
    for _ in 0..2 {
        assert_eq!(
            tuned(&h, "nudge-1", same.clone())
                .send()
                .await
                .unwrap()
                .status(),
            200
        );
        let (_, _, body) = h.upstream.last();
        assert_eq!(body["messages"], hi());
    }
    // The third trips the guard: with nudge on, the run goes on and the agent is told.
    assert_eq!(
        tuned(&h, "nudge-1", same.clone())
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    let (_, _, body) = h.upstream.last();
    let note = body["messages"][0]["content"][1]["text"]
        .as_str()
        .unwrap_or("");
    assert!(
        note.starts_with("[Tokenrein]") && note.contains("same call"),
        "{body}"
    );
    // The first message itself is unchanged, so the cached prefix still matches.
    assert_eq!(body["messages"][0]["content"][0]["text"], "hi");
    // Carrying on the same way: three more and the run is paused.
    for _ in 0..2 {
        assert_eq!(
            tuned(&h, "nudge-1", same.clone())
                .send()
                .await
                .unwrap()
                .status(),
            200
        );
    }
    let resp = tuned(&h, "nudge-1", same.clone()).send().await.unwrap();
    assert_eq!(resp.status(), 403);
    let v: Value = resp.json().await.unwrap();
    assert_eq!(v["error"]["type"], "tokenrein_run_paused");
}

#[tokio::test]
async fn growing_context_without_new_information_is_caught() {
    let Some(h) = Harness::start().await else {
        return;
    };
    let report = |run: &'static str, input: u64| {
        h.http
            .post(format!("{}/v1/usage", h.proxy))
            .bearer_auth(TUNED_KEY)
            .json(
                &json!({"events": [{"run_id": run, "agent": "coder", "provider": "anthropic",
                "model": "claude-haiku-4-5", "input_tokens": input, "output_tokens": 10}]}),
            )
            .send()
    };
    let step = |run: &'static str, input: Value| {
        h.http
            .post(format!("{}/v1/steps", h.proxy))
            .bearer_auth(TUNED_KEY)
            .json(
                &json!({"steps": [{"run_id": run, "agent": "coder", "kind": "tool_call",
                "name": "Read", "content": input}]}),
            )
            .send()
    };
    // Each call adds 10k tokens of context. With a new file read between calls, that's progress.
    for i in 0..8u64 {
        assert_eq!(
            step("np-ok", json!({"path": format!("src/{i}.rs")}))
                .await
                .unwrap()
                .status(),
            200
        );
        assert_eq!(
            report("np-ok", 10_000 * (i + 1)).await.unwrap().status(),
            200
        );
    }
    assert!(
        tokenrein::loops::take_nudge(&h.state, "acme", "np-ok")
            .await
            .is_none()
    );
    // The same growth with nothing new: after 4 calls the agent is nudged.
    for i in 0..6u64 {
        assert_eq!(
            report("np-stuck", 10_000 * (i + 1)).await.unwrap().status(),
            200
        );
    }
    let note = tokenrein::loops::take_nudge(&h.state, "acme", "np-stuck")
        .await
        .unwrap();
    assert!(note.contains("without new information"), "{note}");
}

#[tokio::test]
async fn agents_can_read_their_budget_over_mcp() {
    let Some(h) = Harness::start().await else {
        return;
    };
    let rpc = |key: &'static str, body: Value| {
        h.http
            .post(format!("{}/mcp", h.proxy))
            .bearer_auth(key)
            .json(&body)
            .send()
    };
    let init: Value = rpc(TUNED_KEY, json!({"jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "t", "version": "1"}}}))
    .await
    .unwrap()
    .json()
    .await
    .unwrap();
    assert_eq!(init["result"]["serverInfo"]["name"], "tokenrein");
    let resp = rpc(
        TUNED_KEY,
        json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
    )
    .await
    .unwrap();
    assert_eq!(resp.status(), 202);
    let tools: Value = rpc(
        TUNED_KEY,
        json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}),
    )
    .await
    .unwrap()
    .json()
    .await
    .unwrap();
    assert_eq!(tools["result"]["tools"][0]["name"], "tokenrein_budget");
    // A run spends some of its $1 budget, then asks what is left.
    assert_eq!(
        tuned(
            &h,
            "mcp-run",
            json!({"model": "claude-haiku-4-5", "max_tokens": 50, "messages": hi()})
        )
        .send()
        .await
        .unwrap()
        .status(),
        200
    );
    let call: Value = rpc(
        TUNED_KEY,
        json!({"jsonrpc": "2.0", "id": 3, "method": "tools/call",
        "params": {"name": "tokenrein_budget", "arguments": {"run_id": "mcp-run"}}}),
    )
    .await
    .unwrap()
    .json()
    .await
    .unwrap();
    let r = &call["result"];
    assert_eq!(r["isError"], false, "{call}");
    assert!(
        r["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("Run mcp-run"),
        "{call}"
    );
    let b = &r["structuredContent"]["run_budget"];
    assert_eq!(b["limit_usd"], 1.0);
    assert!(b["spent_usd"].as_f64().unwrap() > 0.0);
    assert!(b["left_usd"].as_f64().unwrap() < 1.0);
    // Without a run id it finds the agent's latest run in the project.
    let call: Value = rpc(
        TUNED_KEY,
        json!({"jsonrpc": "2.0", "id": 4, "method": "tools/call",
        "params": {"name": "tokenrein_budget", "arguments": {}}}),
    )
    .await
    .unwrap()
    .json()
    .await
    .unwrap();
    assert_eq!(
        call["result"]["structuredContent"]["run"], "mcp-run",
        "{call}"
    );
    // Unknown methods and tools, and a missing key.
    let unknown: Value = rpc(
        TUNED_KEY,
        json!({"jsonrpc": "2.0", "id": 5, "method": "resources/list"}),
    )
    .await
    .unwrap()
    .json()
    .await
    .unwrap();
    assert_eq!(unknown["error"]["code"], -32601);
    let resp = h
        .http
        .post(format!("{}/mcp", h.proxy))
        .json(&json!({"jsonrpc": "2.0", "id": 6, "method": "tools/list"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);
}
