//! Coding-agent connectors (Codex CLI, Gemini CLI, Cursor, GitHub Copilot) against a real
//! redis-server: their telemetry, their hooks, the Cursor usage import and the connect scripts.

mod common;

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::State;
use axum::http::HeaderMap;
use axum::routing::post;
use axum::{Json, Router};
use reqwest::{Method, StatusCode};
use serde_json::{Value, json};
use tokenrein::config::{Resolved, sha256_hex};
use tokenrein::{AppState, admin, build_state, control, proxy};

const KEY: &str = "tl_connectors_key_0001";
const ADMIN_TOKEN: &str = "admin-token-0123456789";
const SECRETS_KEY: &str = "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f";
const CURSOR_ADMIN_KEY: &str = "key_cursor_admin_test_0001";

/// What the fake Cursor Admin API received: (Authorization header, body).
type Seen = Arc<Mutex<Vec<(Option<String>, Value)>>>;

async fn serve(router: Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    format!("http://{addr}")
}

/// Cursor's `POST /teams/filtered-usage-events`, with the sample events from its docs on page 1.
async fn fake_cursor(
    State(seen): State<Seen>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    let auth = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    seen.lock().unwrap().push((auth.clone(), body.clone()));
    let expected = format!("Basic {}", base64(&format!("{CURSOR_ADMIN_KEY}:")));
    if auth.as_deref() != Some(expected.as_str()) {
        return (StatusCode::UNAUTHORIZED, "unauthorized").into_response();
    }
    let page = body["page"].as_u64().unwrap_or(1);
    let events = if page == 1 {
        json!([
            {
                "timestamp": "1750979225854",
                "userEmail": "developer@company.com",
                "conversationId": "8f2e4a1b-6c3d-4e5f-9a7b-2d1c8e6f4a3b",
                "model": "claude-4.5-sonnet",
                "kind": "Usage-based",
                "maxMode": true,
                "requestsCosts": 5,
                "isTokenBasedCall": true,
                "isChargeable": true,
                "isHeadless": false,
                "tokenUsage": {"inputTokens": 126, "outputTokens": 450, "cacheWriteTokens": 6112,
                               "cacheReadTokens": 11964, "totalCents": 20.18232},
                "chargedCents": 21.36232,
                "cursorTokenFee": 1.18
            },
            {
                "timestamp": "1750979173824",
                "userEmail": "developer@company.com",
                "conversationId": "8f2e4a1b-6c3d-4e5f-9a7b-2d1c8e6f4a3b",
                "model": "claude-4.5-sonnet",
                "kind": "Usage-based",
                "maxMode": true,
                "requestsCosts": 10,
                "isTokenBasedCall": true,
                "isChargeable": true,
                "isHeadless": false,
                "tokenUsage": {"inputTokens": 5805, "outputTokens": 311, "cacheWriteTokens": 11964,
                               "cacheReadTokens": 0, "totalCents": 40.167, "discountPercentOff": 10},
                "chargedCents": 37.33,
                "cursorTokenFee": 1.18
            },
            {
                "timestamp": "1750978339901",
                "userEmail": "admin@company.com",
                "model": "claude-4-sonnet-thinking",
                "kind": "Included in Business",
                "maxMode": true,
                "requestsCosts": 1.4,
                "isTokenBasedCall": false,
                "isChargeable": false,
                "isHeadless": false,
                "chargedCents": 8
            }
        ])
    } else {
        json!([{
            "timestamp": "1750970000000",
            "userEmail": "dev2@company.com",
            "conversationId": "c2",
            "model": "gpt-5",
            "kind": "Usage-based",
            "isTokenBasedCall": true,
            "isChargeable": true,
            "tokenUsage": {"inputTokens": 1000, "outputTokens": 100, "cacheWriteTokens": 0,
                           "cacheReadTokens": 0, "totalCents": 1.5},
            "chargedCents": 1.5
        }])
    };
    Json(json!({
        "totalUsageEventsCount": 4,
        "pagination": {"numPages": 2, "currentPage": page, "pageSize": 500,
                       "hasNextPage": page == 1, "hasPreviousPage": page > 1},
        "usageEvents": events,
    }))
    .into_response()
}

fn base64(s: &str) -> String {
    const T: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let b = s.as_bytes();
    let mut out = String::new();
    for chunk in b.chunks(3) {
        let n = chunk.iter().fold(0u32, |n, &c| (n << 8) | c as u32) << (8 * (3 - chunk.len()));
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(T[((n >> (18 - 6 * i)) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

fn config(redis: &str, cursor: &str) -> String {
    format!(
        r#"
        [server]
        listen = "127.0.0.1:0"
        admin_token_env = "ADMIN"
        secrets_key_env = "SECRETS"
        cursor_api_url = "{cursor}"
        [redis]
        url = "{redis}"
        [[orgs]]
        id = "acme"
        [[orgs.projects]]
        id = "agents"
        virtual_key_hashes = ["{key}"]
        [[orgs.projects]]
        id = "cursor"
        "#,
        key = sha256_hex(KEY),
    )
}

struct H {
    _redis: common::RedisServer,
    _state: Arc<AppState>,
    proxy: String,
    admin: String,
    cursor_seen: Seen,
    http: reqwest::Client,
}

impl H {
    async fn start() -> Option<Self> {
        let redis = common::RedisServer::start()?;
        let cursor_seen = Seen::default();
        let cursor = serve(
            Router::new()
                .route("/teams/filtered-usage-events", post(fake_cursor))
                .with_state(cursor_seen.clone()),
        )
        .await;
        let rt = Resolved::from_toml(&config(&redis.url, &cursor), |name| match name {
            "ADMIN" => Some(ADMIN_TOKEN.into()),
            "SECRETS" => Some(SECRETS_KEY.into()),
            other => common::licensed(other),
        })
        .unwrap();
        let state = build_state(rt).await.unwrap();
        control::init(&state).await.unwrap();
        let proxy = serve(proxy::router(state.clone())).await;
        let admin = serve(admin::router(state.clone())).await;
        Some(Self {
            _redis: redis,
            _state: state,
            proxy,
            admin,
            cursor_seen,
            http: reqwest::Client::new(),
        })
    }

    async fn post(&self, path: &str, body: Value) -> (StatusCode, Value) {
        let resp = self
            .http
            .post(format!("{}{path}", self.proxy))
            .bearer_auth(KEY)
            .json(&body)
            .send()
            .await
            .unwrap();
        let status = resp.status();
        (status, resp.json().await.unwrap_or(Value::Null))
    }

    async fn admin(&self, method: Method, path: &str, body: Option<Value>) -> (StatusCode, Value) {
        let mut req = self
            .http
            .request(method, format!("{}{path}", self.admin))
            .bearer_auth(ADMIN_TOKEN);
        if let Some(b) = body {
            req = req.json(&b);
        }
        let resp = req.send().await.unwrap();
        let status = resp.status();
        (status, resp.json().await.unwrap_or(Value::Null))
    }

    async fn get(&self, path: &str) -> Value {
        let (status, body) = self.admin(Method::GET, path, None).await;
        assert_eq!(status, StatusCode::OK, "{path}: {body}");
        body
    }

    async fn pause(&self, run: &str) {
        let (s, _) = self
            .admin(
                Method::POST,
                &format!("/admin/orgs/acme/runs/{run}/pause"),
                None,
            )
            .await;
        assert_eq!(s, StatusCode::OK);
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

    async fn timeline(&self, run: &str, events: usize) -> Value {
        self.eventually(&format!("/admin/orgs/acme/runs/{run}/timeline"), |b| {
            b["events"].as_array().is_some_and(|e| e.len() == events)
        })
        .await
    }
}

fn s(k: &str, v: &str) -> Value {
    json!({"key": k, "value": {"stringValue": v}})
}
fn i(k: &str, v: i64) -> Value {
    json!({"key": k, "value": {"intValue": v.to_string()}})
}

/// Codex CLI's OTLP/JSON log export: `codex.sse_event` with `event.kind = response.completed`
/// carries the token counts; the session metadata comes on every event.
fn codex_logs(conversation: &str) -> Value {
    let meta = |name: &str| {
        vec![
            s("event.name", name),
            s("event.timestamp", "2026-10-22T10:00:00.000Z"),
            s("conversation.id", conversation),
            s("app.version", "0.130.0"),
            s("auth_mode", "ApiKey"),
            s("originator", "codex_cli_rs"),
            s("user.account_id", "acct_123"),
            s("user.email", "dev@example.com"),
            s("terminal.type", "iTerm.app"),
            s("model", "gpt-5-codex"),
            s("slug", "gpt-5-codex"),
        ]
    };
    let mut completed = meta("codex.sse_event");
    completed.extend([
        s("event.kind", "response.completed"),
        // Codex formats some counts with Display, so they arrive as strings.
        s("input_token_count", "12000"),
        s("output_token_count", "900"),
        i("cached_token_count", 8000),
        i("cache_write_token_count", 0),
        i("reasoning_token_count", 400),
        s("tool_token_count", "12900"),
    ]);
    let mut delta = meta("codex.sse_event");
    delta.extend([
        s("event.kind", "response.output_text.delta"),
        s("duration_ms", "3"),
    ]);
    let mut prompt = meta("codex.user_prompt");
    prompt.extend([i("prompt_length", 42), s("prompt", "[REDACTED]")]);
    json!({"resourceLogs": [{
        "resource": {"attributes": [
            s("service.name", "codex_cli_rs"), s("service.version", "0.130.0"), s("env", "tokenrein")
        ]},
        "scopeLogs": [{"scope": {"name": "codex_otel"}, "logRecords": [
            {"timeUnixNano": "1792641600000000000", "attributes": prompt},
            {"timeUnixNano": "1792641601000000000", "attributes": delta},
            {"timeUnixNano": "1792641602000000000", "attributes": completed},
        ]}]
    }]})
}

/// Gemini CLI's OTLP/HTTP log export: `gemini_cli.api_response` plus its semantic-convention
/// copy (`gen_ai.client.inference.operation.details`), which must not be counted twice.
fn gemini_logs(session: &str, role: &str) -> Value {
    let common = || {
        vec![
            s("session.id", session),
            s("installation.id", "inst-1"),
            json!({"key": "interactive", "value": {"boolValue": true}}),
            s("user.email", "dev@example.com"),
            s("auth_type", "gemini-api-key"),
        ]
    };
    let mut response = common();
    response.extend([
        s("event.name", "gemini_cli.api_response"),
        s("event.timestamp", "2026-10-22T10:00:00.000Z"),
        s("model", "gemini-2.5-pro"),
        json!({"key": "duration_ms", "value": {"intValue": 2100}}),
        json!({"key": "input_token_count", "value": {"intValue": 5000}}),
        json!({"key": "output_token_count", "value": {"intValue": 300}}),
        json!({"key": "cached_content_token_count", "value": {"intValue": 2000}}),
        json!({"key": "thoughts_token_count", "value": {"intValue": 700}}),
        json!({"key": "tool_token_count", "value": {"intValue": 0}}),
        json!({"key": "total_token_count", "value": {"intValue": 6000}}),
        s("prompt_id", "p-1"),
        json!({"key": "status_code", "value": {"intValue": 200}}),
        s("role", role),
    ]);
    let mut semantic = common();
    semantic.extend([
        s("event.name", "gen_ai.client.inference.operation.details"),
        json!({"key": "gen_ai.usage.input_tokens", "value": {"intValue": 5000}}),
        json!({"key": "gen_ai.usage.output_tokens", "value": {"intValue": 300}}),
    ]);
    json!({"resourceLogs": [{
        "resource": {"attributes": [s("service.name", "gemini-cli"), s("service.version", "0.34.0")]},
        "scopeLogs": [{"scope": {"name": "gemini-cli"}, "logRecords": [
            {"body": {"stringValue": "API response from gemini-2.5-pro. Status: 200. Duration: 2100ms."},
             "attributes": response},
            {"body": {"stringValue": "GenAI operation details from gemini-2.5-pro."},
             "attributes": semantic},
        ]}]
    }]})
}

#[tokio::test]
async fn codex_telemetry_and_hooks() {
    let Some(h) = H::start().await else { return };
    let (status, _) = h.post("/v1/otel/v1/logs", codex_logs("thr_1")).await;
    assert_eq!(status, StatusCode::OK);
    let tl = h.timeline("codex-thr_1", 1).await;
    let e = &tl["events"][0];
    assert_eq!(e["agent"], "codex");
    assert_eq!(e["source"], "codex");
    assert_eq!(e["user"], "dev@example.com");
    assert_eq!(e["provider"], "openai");
    assert_eq!(e["model"], "gpt-5-codex");
    assert_eq!(e["usage"]["input"], 4000);
    assert_eq!(e["usage"]["cached_input"], 8000);
    assert_eq!(e["usage"]["output"], 900);
    // gpt-5: 4000 * $1.25/M + 8000 * $0.125/M + 900 * $10/M
    assert_eq!(e["actual_micros"], 15_000);
    assert_eq!(e["price_known"], true);

    let hook = |event: &str| {
        json!({"session_id": "thr_1", "transcript_path": null, "cwd": "/repo",
               "hook_event_name": event, "model": "gpt-5-codex", "turn_id": "t1",
               "permission_mode": "default", "tool_name": "Bash", "tool_use_id": "call_1",
               "tool_input": {"command": "cargo test"}, "prompt": "fix the build"})
    };
    let (s, body) = h.post("/v1/hooks/codex", hook("UserPromptSubmit")).await;
    assert_eq!((s, body), (StatusCode::OK, json!({})));
    h.pause("codex-thr_1").await;
    let (_, body) = h.post("/v1/hooks/codex", hook("UserPromptSubmit")).await;
    assert_eq!(body["decision"], "block");
    assert!(body["reason"].as_str().unwrap().contains("paused"));
    let (_, body) = h.post("/v1/hooks/codex", hook("PreToolUse")).await;
    assert_eq!(body["hookSpecificOutput"]["hookEventName"], "PreToolUse");
    assert_eq!(body["hookSpecificOutput"]["permissionDecision"], "deny");
    // Events that can't block never do.
    let (_, body) = h.post("/v1/hooks/codex", hook("Stop")).await;
    assert_eq!(body, json!({}));
    let (_, body) = h.post("/v1/hooks/codex", hook("SubagentStart")).await;
    assert_eq!(body, json!({}));
    // The key is required.
    let resp = h
        .http
        .post(format!("{}/v1/hooks/codex", h.proxy))
        .json(&hook("PreToolUse"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn gemini_telemetry_and_hooks() {
    let Some(h) = H::start().await else { return };
    let (status, _) = h.post("/v1/otel/v1/logs", gemini_logs("g-1", "main")).await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = h
        .post("/v1/otel/v1/logs", gemini_logs("g-1", "subagent"))
        .await;
    assert_eq!(status, StatusCode::OK);
    let tl = h.timeline("gemini-g-1", 2).await;
    let runs: Vec<&str> = tl["runs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["run_id"].as_str().unwrap())
        .collect();
    assert_eq!(runs, ["gemini-g-1", "gemini-g-1-sub"]);
    let e = &tl["events"][0];
    assert_eq!(e["agent"], "gemini-cli");
    assert_eq!(e["source"], "gemini-cli");
    assert_eq!(e["provider"], "google");
    assert_eq!(e["usage"]["input"], 3000);
    assert_eq!(e["usage"]["cached_input"], 2000);
    // Thinking tokens are billed as output.
    assert_eq!(e["usage"]["output"], 1000);
    // gemini-2.5-pro: 3000 * $1.25/M + 2000 * $0.31/M + 1000 * $10/M
    assert_eq!(e["actual_micros"], 14_370);
    assert_eq!(tl["runs"][0]["budget"]["spent_micros"], 28_740);

    // Gemini CLI exports traces and metrics next to the logs over OTLP/HTTP.
    for path in ["/v1/otel/v1/traces", "/v1/otel/v1/metrics"] {
        let (status, _) = h.post(path, json!({"resourceSpans": []})).await;
        assert_eq!(status, StatusCode::OK, "{path}");
    }

    let hook = |event: &str| {
        json!({"session_id": "g-1", "transcript_path": "/tmp/t.json", "cwd": "/repo",
               "hook_event_name": event, "timestamp": "2026-10-22T10:00:00.000Z",
               "tool_name": "run_shell_command", "tool_input": {"command": "ls"},
               "prompt": "hi", "llm_request": {"model": "gemini-2.5-pro", "messages": []}})
    };
    let (_, body) = h.post("/v1/hooks/gemini", hook("BeforeTool")).await;
    assert_eq!(body, json!({}));
    h.pause("gemini-g-1").await;
    for event in ["BeforeAgent", "BeforeTool", "BeforeModel"] {
        let (_, body) = h.post("/v1/hooks/gemini", hook(event)).await;
        assert_eq!(body["decision"], "deny", "{event}");
        assert!(body["reason"].as_str().unwrap().contains("paused"));
    }
    let (_, body) = h.post("/v1/hooks/gemini", hook("AfterAgent")).await;
    assert_eq!(body, json!({}));
}

#[tokio::test]
async fn cursor_hooks() {
    let Some(h) = H::start().await else { return };
    let base = |event: &str| {
        json!({"conversation_id": "conv-1", "generation_id": "gen-1",
               "model": "claude-opus-4-7-thinking-max", "hook_event_name": event,
               "cursor_version": "2.1.0", "workspace_roots": ["/repo"],
               "user_email": "dev@example.com", "transcript_path": null})
    };
    let mut prompt = base("beforeSubmitPrompt");
    prompt["prompt"] = json!("refactor this");
    prompt["attachments"] = json!([]);
    let mut tool = base("preToolUse");
    tool["tool_name"] = json!("Shell");
    tool["tool_input"] = json!({"command": "npm install", "working_directory": "/repo"});
    tool["tool_use_id"] = json!("abc123");
    let mut sub = base("subagentStart");
    sub["subagent_id"] = json!("abc-123");
    sub["subagent_type"] = json!("generalPurpose");
    sub["parent_conversation_id"] = json!("conv-1");
    sub.as_object_mut().unwrap().remove("conversation_id");

    let (_, body) = h.post("/v1/hooks/cursor", prompt.clone()).await;
    assert_eq!(body, json!({"continue": true}));
    let (_, body) = h.post("/v1/hooks/cursor", tool.clone()).await;
    assert_eq!(body, json!({"permission": "allow"}));
    let runs = h
        .eventually("/admin/orgs/acme/runs", |b| {
            b["runs"]
                .as_array()
                .unwrap()
                .iter()
                .any(|r| r["run_id"] == "cursor-conv-1")
        })
        .await;
    let r = runs["runs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["run_id"] == "cursor-conv-1")
        .unwrap()
        .clone();
    assert_eq!(r["agent"], "cursor");

    h.pause("cursor-conv-1").await;
    let (_, body) = h.post("/v1/hooks/cursor", prompt).await;
    assert_eq!(body["continue"], false);
    assert!(body["user_message"].as_str().unwrap().contains("paused"));
    let (_, body) = h.post("/v1/hooks/cursor", tool).await;
    assert_eq!(body["permission"], "deny");
    assert!(body["agent_message"].as_str().is_some());
    let (_, body) = h.post("/v1/hooks/cursor", sub).await;
    assert_eq!(body["permission"], "deny");
    let (_, body) = h.post("/v1/hooks/cursor", base("afterAgentResponse")).await;
    assert_eq!(body, json!({}));
}

/// Copilot's OTLP/JSON trace export, built from the span attributes in GitHub's docs (Copilot
/// CLI's command reference and VS Code's agent monitoring guide), not captured from a client:
/// an `invoke_agent` span over `chat` spans. The CLI puts the session on every span; VS Code's
/// `chat` spans leave it to the `invoke_agent` span.
fn copilot_traces(session: &str) -> Value {
    const TRACE: &str = "5b8efff798038103d269b633813fc60c";
    let span = |id: &str, name: &str, attrs: Vec<Value>| {
        json!({"traceId": TRACE, "spanId": id, "parentSpanId": "eee19b7ec3c1b174",
               "name": name, "kind": 3, "startTimeUnixNano": "1792641600000000000",
               "endTimeUnixNano": "1792641602500000000", "attributes": attrs,
               "status": {"code": 1}})
    };
    let chat = |model: &str, input: i64, cached: i64, with_session: bool| {
        let mut a = vec![
            s("gen_ai.operation.name", "chat"),
            s("gen_ai.provider.name", "github"),
            s("gen_ai.request.model", model),
            s("gen_ai.response.model", model),
            s("gen_ai.response.id", "chatcmpl-1"),
            json!({"key": "gen_ai.response.finish_reasons",
                   "value": {"arrayValue": {"values": [{"stringValue": "tool_calls"}]}}}),
            i("gen_ai.usage.input_tokens", input),
            i("gen_ai.usage.output_tokens", 500),
            i("gen_ai.usage.cache_read.input_tokens", cached),
            i("gen_ai.usage.cache_creation.input_tokens", 1000),
            json!({"key": "github.copilot.cost", "value": {"doubleValue": 1.0}}),
            i("github.copilot.nano_aiu", 2_500_000),
            s("github.copilot.turn_id", "t-1"),
            s("server.address", "api.githubcopilot.com"),
        ];
        if with_session {
            a.push(s("gen_ai.conversation.id", session));
        }
        a
    };
    let agent = vec![
        s("gen_ai.operation.name", "invoke_agent"),
        s("gen_ai.agent.id", "github.copilot.default"),
        s("gen_ai.conversation.id", session),
        s("gen_ai.request.model", "claude-sonnet-4.5"),
        s("user.name", "octocat"),
        // The invocation's totals: not counted again.
        i("gen_ai.usage.input_tokens", 20000),
        i("gen_ai.usage.output_tokens", 1000),
    ];
    let tool = vec![
        s("gen_ai.operation.name", "execute_tool"),
        s("gen_ai.tool.name", "bash"),
        s("gen_ai.tool.call.id", "call_1"),
    ];
    // Plain GenAI conventions, without Copilot's own attributes.
    let other: Vec<Value> = chat("gemini-2.5-pro", 9000, 0, true)
        .into_iter()
        .filter(|a| !a["key"].as_str().unwrap().starts_with("github.copilot."))
        .collect();
    json!({"resourceSpans": [
        {"resource": {"attributes": [s("service.name", "github-copilot"), s("service.version", "1.0.40")]},
         "scopeSpans": [{"scope": {"name": "github.copilot", "version": "1.0.40"}, "spans": [
             span("eee19b7ec3c1b174", "invoke_agent", agent),
             span("eee19b7ec3c1b175", "chat claude-sonnet-4.5", chat("claude-sonnet-4.5", 10000, 8000, true)),
             span("eee19b7ec3c1b176", "execute_tool bash", tool),
         ]}]},
        {"resource": {"attributes": [s("service.name", "copilot-chat"), s("session.id", "vscode-window-1")]},
         "scopeSpans": [{"scope": {"name": "copilot-chat"}, "spans": [
             span("eee19b7ec3c1b177", "chat gpt-5", chat("gpt-5", 3000, 0, false)),
         ]}]},
        // A GenAI span from another agent's trace export is not Copilot's.
        {"resource": {"attributes": [s("service.name", "gemini-cli")]},
         "scopeSpans": [{"scope": {"name": "gemini-cli"}, "spans": [
             span("eee19b7ec3c1b178", "chat", other),
         ]}]},
    ]})
}

#[tokio::test]
async fn copilot_telemetry_and_hooks() {
    let Some(h) = H::start().await else { return };
    let (status, body) = h.post("/v1/otel/v1/traces", copilot_traces("cs-1")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let tl = h.timeline("copilot-cs-1", 2).await;
    let events = tl["events"].as_array().unwrap();
    let claude = events
        .iter()
        .find(|e| e["provider"] == "anthropic")
        .unwrap();
    assert_eq!(claude["agent"], "copilot");
    assert_eq!(claude["source"], "copilot");
    assert_eq!(claude["user"], "octocat");
    assert_eq!(claude["model"], "claude-sonnet-4-5");
    assert_eq!(claude["usage"]["input"], 1000);
    assert_eq!(claude["usage"]["cached_input"], 8000);
    assert_eq!(claude["usage"]["cache_write"], 1000);
    assert_eq!(claude["upstream_ms"], 2500);
    // claude-sonnet-4: 1000 * $3/M + 8000 * $0.30/M + 1000 * $3.75/M + 500 * $15/M
    assert_eq!(claude["actual_micros"], 16_650);
    assert_eq!(claude["price_known"], true);
    // VS Code's chat span: the session and user come from the trace's invoke_agent span.
    let gpt = events.iter().find(|e| e["provider"] == "openai").unwrap();
    assert_eq!(gpt["user"], "octocat");
    assert_eq!(gpt["usage"]["input"], 2000);
    let spent = tl["runs"][0]["budget"]["spent_micros"].clone();
    // An exporter retrying the same batch charges nothing twice.
    let (status, _) = h.post("/v1/otel/v1/traces", copilot_traces("cs-1")).await;
    assert_eq!(status, StatusCode::OK);
    let tl = h.timeline("copilot-cs-1", 2).await;
    assert_eq!(tl["runs"][0]["budget"]["spent_micros"], spent);
    // Only OTLP/JSON is read.
    let resp = h
        .http
        .post(format!("{}/v1/otel/v1/traces", h.proxy))
        .bearer_auth(KEY)
        .header("content-type", "application/x-protobuf")
        .body(vec![0u8; 4])
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);

    // Native hook payloads, with the event name the Tokenrein hook adds.
    let hook = |event: &str| {
        json!({"hook_event_name": event, "sessionId": "cs-1", "timestamp": 1792641600000_i64,
               "cwd": "/repo", "toolName": "bash", "toolArgs": {"command": "cargo test"},
               "prompt": "fix the build"})
    };
    let (s, body) = h.post("/v1/hooks/copilot", hook("preToolUse")).await;
    assert_eq!((s, body), (StatusCode::OK, json!({})));
    h.pause("copilot-cs-1").await;
    let (_, body) = h.post("/v1/hooks/copilot", hook("preToolUse")).await;
    assert_eq!(body["permissionDecision"], "deny");
    assert!(
        body["permissionDecisionReason"]
            .as_str()
            .unwrap()
            .contains("paused")
    );
    // VS Code-compatible payloads are read the same way.
    let vscode = json!({"hook_event_name": "PreToolUse", "session_id": "cs-1",
                        "timestamp": "2026-10-31T10:00:00.000Z", "cwd": "/repo",
                        "tool_name": "Bash", "tool_input": {"command": "ls"}});
    let (_, body) = h.post("/v1/hooks/copilot", vscode).await;
    assert_eq!(body["permissionDecision"], "deny");
    // Copilot drops prompt hooks' answers, so they never block.
    let (_, body) = h
        .post("/v1/hooks/copilot", hook("userPromptSubmitted"))
        .await;
    assert_eq!(body, json!({}));
}

#[tokio::test]
async fn cursor_usage_import() {
    let Some(h) = H::start().await else { return };
    let path = "/admin/orgs/acme/connectors/cursor";
    let st = h.get(path).await;
    assert_eq!(st["configured"], false);
    let (s, _) = h.admin(Method::POST, &format!("{path}/sync"), None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    // A key is needed the first time, and the project must exist.
    let (s, _) = h
        .admin(Method::PUT, path, Some(json!({"project": "cursor"})))
        .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let (s, _) = h
        .admin(
            Method::PUT,
            path,
            Some(json!({"project": "nope", "api_key": CURSOR_ADMIN_KEY})),
        )
        .await;
    assert_eq!(s, StatusCode::NOT_FOUND);

    let (s, body) = h
        .admin(
            Method::PUT,
            path,
            Some(json!({"project": "cursor", "api_key": CURSOR_ADMIN_KEY})),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{body}");
    assert_eq!(body["configured"], true);
    assert_eq!(body["last4"], "0001");
    // The key is stored encrypted and never returned.
    assert!(!body.to_string().contains(CURSOR_ADMIN_KEY));
    let audit = h.get("/admin/audit?org=acme").await;
    assert!(!audit.to_string().contains(CURSOR_ADMIN_KEY));

    let (s, body) = h.admin(Method::POST, &format!("{path}/sync"), None).await;
    assert_eq!(s, StatusCode::OK, "{body}");
    assert_eq!(body["imported"], 4);
    {
        let seen = h.cursor_seen.lock().unwrap();
        assert_eq!(seen.len(), 2, "two pages");
        assert_eq!(seen[1].1["page"], 2);
        let start = seen[0].1["startDate"].as_i64().unwrap();
        let end = seen[0].1["endDate"].as_i64().unwrap();
        // The first import starts at the beginning of today (UTC).
        assert_eq!(start % 86_400_000, 0);
        assert!(end - start <= 86_400_000);
    }
    let tl = h
        .timeline("cursor-8f2e4a1b-6c3d-4e5f-9a7b-2d1c8e6f4a3b", 2)
        .await;
    let e = &tl["events"][0];
    assert_eq!(e["source"], "cursor");
    assert_eq!(e["agent"], "cursor");
    assert_eq!(e["project"], "cursor");
    assert_eq!(e["user"], "developer@company.com");
    // chargedCents: 21.36232 + 37.33
    assert_eq!(tl["runs"][0]["budget"]["spent_micros"], 586_923);
    // Usage outside a conversation: one run per user and day.
    h.timeline("cursor-admin-company.com-20250626", 1).await;
    // Imported calls carry their own cost, so their models need no price.
    let models = h.get("/admin/models?org=acme").await;
    assert!(
        !models["unpriced"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["model"] == "claude-4.5-sonnet"),
        "{models}"
    );

    // Reading the same window again charges nothing twice.
    let (_, body) = h.admin(Method::POST, &format!("{path}/sync"), None).await;
    assert_eq!(body["imported"], 0);
    let st = h.get(path).await;
    assert_eq!(st["sync"]["imported"], 0);
    assert!(st["sync"]["through_ms"].as_i64().is_some());
    // The next read overlaps the last one, for events Cursor reports late.
    {
        let seen = h.cursor_seen.lock().unwrap();
        let first_end = seen[0].1["endDate"].as_i64().unwrap();
        let start = seen.last().unwrap().1["startDate"].as_i64().unwrap();
        assert!(start < first_end);
    }

    // A rejected key is reported.
    let (s, _) = h
        .admin(
            Method::PUT,
            path,
            Some(json!({"project": "cursor", "api_key": "key_wrong_0000"})),
        )
        .await;
    assert_eq!(s, StatusCode::OK);
    let (s, body) = h.admin(Method::POST, &format!("{path}/sync"), None).await;
    assert_eq!(s, StatusCode::CONFLICT);
    assert!(body["error"].as_str().unwrap().contains("rejected"));
    assert!(
        h.get(path).await["sync"]["error"]
            .as_str()
            .unwrap()
            .contains("rejected")
    );

    let (s, _) = h.admin(Method::DELETE, path, None).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    assert_eq!(h.get(path).await["configured"], false);
}

// --- Connect scripts, run for real with python3 against a throwaway home directory ---

struct Home(PathBuf);

impl Home {
    fn new() -> Self {
        let p = std::env::temp_dir().join(format!("tl-home-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&p).unwrap();
        Self(p)
    }
    fn path(&self, rel: &str) -> PathBuf {
        self.0.join(rel)
    }
    fn read(&self, rel: &str) -> String {
        std::fs::read_to_string(self.path(rel)).unwrap_or_default()
    }
    fn write(&self, rel: &str, text: &str) {
        let p = self.path(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, text).unwrap();
    }
    fn backups(&self, dir: &str) -> usize {
        std::fs::read_dir(self.path(dir))
            .map(|d| {
                d.flatten()
                    .filter(|e| e.file_name().to_string_lossy().contains(".bak-tokenrein-"))
                    .count()
            })
            .unwrap_or(0)
    }
}

impl Drop for Home {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

async fn have_python() -> bool {
    let ok = tokio::process::Command::new("python3")
        .arg("--version")
        .output()
        .await
        .is_ok_and(|o| o.status.success());
    if !ok {
        eprintln!("skipping: python3 not found");
    }
    ok
}

/// Run a command with HOME set to `home`, feeding `stdin`; returns (exit code, stdout, stderr).
async fn run(home: &Home, program: &str, args: &[&str], stdin: &str) -> (i32, String, String) {
    use tokio::io::AsyncWriteExt;
    let mut child = tokio::process::Command::new(program)
        .args(args)
        .env("HOME", &home.0)
        .env_remove("CODEX_HOME")
        .env_remove("COPILOT_HOME")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    input.write_all(stdin.as_bytes()).await.unwrap();
    drop(input);
    let out = child.wait_with_output().await.unwrap();
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

impl H {
    /// Download a connect script into `home`.
    async fn script(&self, home: &Home, tool: &str) -> String {
        let resp = self
            .http
            .get(format!("{}/connect/{tool}.py", self.admin))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let path = home.path(&format!("{tool}-connect.py"));
        std::fs::write(&path, resp.text().await.unwrap()).unwrap();
        path.to_string_lossy().into_owned()
    }
}

/// Run the hook command an agent would run, with `event` on stdin; returns its JSON answer.
async fn run_hook(home: &Home, command: &str, event: Value) -> Value {
    let (code, out, err) = run(home, "sh", &["-c", command], &event.to_string()).await;
    assert_eq!(code, 0, "hook failed: {err}");
    serde_json::from_str(out.trim()).unwrap_or_else(|_| panic!("hook printed {out:?} ({err})"))
}

#[tokio::test]
async fn codex_connect_script() {
    let Some(h) = H::start().await else { return };
    if !have_python().await {
        return;
    }
    let home = Home::new();
    let original = "model = \"gpt-5-codex\"\n\n[profiles.fast]\nmodel = \"gpt-5-mini\"\n";
    home.write(".codex/config.toml", original);
    let script = h.script(&home, "codex").await;
    let args = ["-I", &script, "--key", KEY, "--url", &h.proxy];
    let (code, out, err) = run(&home, "python3", &args, "").await;
    assert_eq!(code, 0, "{out}{err}");
    // Running it again replaces its own settings rather than adding more.
    let (code, _, err) = run(&home, "python3", &args, "").await;
    assert_eq!(code, 0, "{err}");

    let text = home.read(".codex/config.toml");
    let config: toml::Value = toml::from_str(&text).unwrap();
    assert_eq!(config["model"].as_str(), Some("gpt-5-codex"));
    assert_eq!(
        config["profiles"]["fast"]["model"].as_str(),
        Some("gpt-5-mini")
    );
    let http = &config["otel"]["exporter"]["otlp-http"];
    assert_eq!(
        http["endpoint"].as_str().unwrap(),
        format!("{}/v1/otel/v1/logs", h.proxy)
    );
    assert_eq!(http["protocol"].as_str(), Some("json"));
    assert_eq!(
        http["headers"]["Authorization"].as_str().unwrap(),
        format!("Bearer {KEY}")
    );
    assert_eq!(config["otel"]["log_user_prompt"].as_bool(), Some(false));
    assert_eq!(text.matches("[otel]").count(), 1);

    let hooks: Value = serde_json::from_str(&home.read(".codex/hooks.json")).unwrap();
    assert_eq!(hooks["hooks"]["PreToolUse"].as_array().unwrap().len(), 1);
    assert_eq!(hooks["hooks"]["PreToolUse"][0]["matcher"], "*");
    let cmd = hooks["hooks"]["PreToolUse"][0]["hooks"][0]["command"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(cmd.contains(".tokenrein/hook.py"));
    let event = json!({"session_id": "thr_9", "hook_event_name": "PreToolUse", "cwd": "/repo",
                       "turn_id": "t1", "tool_name": "Bash", "tool_input": {"command": "ls"}});
    assert_eq!(run_hook(&home, &cmd, event.clone()).await, json!({}));
    h.pause("codex-thr_9").await;
    let answer = run_hook(&home, &cmd, event).await;
    assert_eq!(answer["hookSpecificOutput"]["permissionDecision"], "deny");
    // The key file is readable by its owner only.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(home.path(".tokenrein/codex.json"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o077, 0);
    }

    let (code, _, err) = run(&home, "python3", &["-I", &script, "--remove"], "").await;
    assert_eq!(code, 0, "{err}");
    assert_eq!(home.read(".codex/config.toml"), original);
    let hooks: Value = serde_json::from_str(&home.read(".codex/hooks.json")).unwrap();
    assert_eq!(hooks, json!({}));
    assert!(!home.path(".tokenrein/codex.json").exists());
    assert!(!home.path(".tokenrein/hook.py").exists());
    assert!(home.backups(".codex") >= 2);

    // Another OpenTelemetry collector is left alone.
    let foreign = "[otel]\nexporter = { otlp-http = { endpoint = \"https://otel.example.com/v1/logs\", protocol = \"binary\" } }\n";
    home.write(".codex/config.toml", foreign);
    let (code, _, err) = run(&home, "python3", &args, "").await;
    assert_eq!(code, 1);
    assert!(err.contains("[otel]"), "{err}");
    assert_eq!(home.read(".codex/config.toml"), foreign);
}

#[tokio::test]
async fn gemini_connect_script() {
    let Some(h) = H::start().await else { return };
    if !have_python().await {
        return;
    }
    let home = Home::new();
    let original = json!({"general": {"vimMode": true}, "hooks": {"BeforeTool": [
        {"matcher": "write_file", "hooks": [{"name": "mine", "type": "command", "command": "true"}]}
    ]}});
    home.write(".gemini/settings.json", &original.to_string());
    let script = h.script(&home, "gemini").await;
    let args = [
        "-I",
        &script,
        "--key",
        KEY,
        "--url",
        &h.proxy,
        "--user",
        "ana@example.com",
    ];
    let (code, out, err) = run(&home, "python3", &args, "").await;
    assert_eq!(code, 0, "{out}{err}");
    let settings: Value = serde_json::from_str(&home.read(".gemini/settings.json")).unwrap();
    let t = &settings["telemetry"];
    assert_eq!(t["enabled"], true);
    assert_eq!(t["target"], "local");
    assert_eq!(t["otlpProtocol"], "http");
    assert_eq!(t["otlpEndpoint"], format!("{}/v1/otel", h.proxy));
    assert_eq!(t["otlpHeaders"]["x-tr-user"], "ana@example.com");
    assert_eq!(t["logPrompts"], false);
    assert_eq!(settings["general"]["vimMode"], true);
    // The user's own hook stays; ours is added next to it.
    assert_eq!(settings["hooks"]["BeforeTool"].as_array().unwrap().len(), 2);
    let cmd = settings["hooks"]["BeforeAgent"][0]["hooks"][0]["command"]
        .as_str()
        .unwrap()
        .to_string();
    let event = json!({"session_id": "g-9", "hook_event_name": "BeforeAgent", "cwd": "/repo",
                       "timestamp": "2026-10-22T10:00:00Z", "prompt": "hi"});
    assert_eq!(run_hook(&home, &cmd, event.clone()).await, json!({}));
    h.pause("gemini-g-9").await;
    assert_eq!(run_hook(&home, &cmd, event).await["decision"], "deny");

    let (code, _, err) = run(&home, "python3", &["-I", &script, "--remove"], "").await;
    assert_eq!(code, 0, "{err}");
    let settings: Value = serde_json::from_str(&home.read(".gemini/settings.json")).unwrap();
    assert_eq!(settings, original);
    assert_eq!(home.backups(".gemini"), 2);
}

#[tokio::test]
async fn cursor_connect_script() {
    let Some(h) = H::start().await else { return };
    if !have_python().await {
        return;
    }
    let home = Home::new();
    let script = h.script(&home, "cursor").await;
    let args = ["-I", &script, "--key", KEY, "--url", &h.proxy];
    let (code, out, err) = run(&home, "python3", &args, "").await;
    assert_eq!(code, 0, "{out}{err}");
    let doc: Value = serde_json::from_str(&home.read(".cursor/hooks.json")).unwrap();
    assert_eq!(doc["version"], 1);
    for event in [
        "beforeSubmitPrompt",
        "preToolUse",
        "subagentStart",
        "sessionEnd",
    ] {
        assert!(doc["hooks"][event][0]["command"].is_string(), "{event}");
    }
    let cmd = doc["hooks"]["beforeSubmitPrompt"][0]["command"]
        .as_str()
        .unwrap()
        .to_string();
    let event = json!({"conversation_id": "conv-9", "generation_id": "g",
                       "hook_event_name": "beforeSubmitPrompt", "prompt": "hi",
                       "user_email": "dev@example.com", "workspace_roots": ["/repo"]});
    assert_eq!(
        run_hook(&home, &cmd, event.clone()).await,
        json!({"continue": true})
    );
    // Tokenrein unreachable: the hook lets everything through, in Cursor's format.
    let cfg_path = home.path(".tokenrein/cursor.json");
    let mut cfg: Value = serde_json::from_str(&home.read(".tokenrein/cursor.json")).unwrap();
    cfg["url"] = json!("http://127.0.0.1:9");
    std::fs::write(&cfg_path, cfg.to_string()).unwrap();
    let mut tool = event.clone();
    tool["hook_event_name"] = json!("preToolUse");
    assert_eq!(
        run_hook(&home, &cmd, tool).await,
        json!({"permission": "allow"})
    );

    let (code, _, err) = run(&home, "python3", &["-I", &script, "--remove"], "").await;
    assert_eq!(code, 0, "{err}");
    let doc: Value = serde_json::from_str(&home.read(".cursor/hooks.json")).unwrap();
    assert_eq!(doc, json!({"version": 1, "hooks": {}}));
    assert!(!Path::new(&cfg_path).exists());
}

#[tokio::test]
async fn copilot_connect_script() {
    let Some(h) = H::start().await else { return };
    if !have_python().await {
        return;
    }
    let home = Home::new();
    let zshrc = "export EDITOR=vim\n";
    home.write(".zshrc", zshrc);
    let script = h.script(&home, "copilot").await;
    let args = ["-I", &script, "--key", KEY, "--url", &h.proxy];
    let (code, out, err) = run(&home, "python3", &args, "").await;
    assert_eq!(code, 0, "{out}{err}");
    // Running it again replaces its own settings rather than adding more.
    let (code, _, err) = run(&home, "python3", &args, "").await;
    assert_eq!(code, 0, "{err}");

    let doc: Value = serde_json::from_str(&home.read(".copilot/hooks/tokenrein.json")).unwrap();
    assert_eq!(doc["version"], 1);
    for event in ["preToolUse", "userPromptSubmitted", "sessionEnd"] {
        let cmd = doc["hooks"][event][0]["bash"].as_str().unwrap();
        assert!(
            cmd.ends_with(&format!(".tokenrein/hook.py copilot {event}")),
            "{cmd}"
        );
        assert_eq!(doc["hooks"][event][0]["type"], "command");
    }
    let zsh = home.read(".zshrc");
    assert!(zsh.starts_with(zshrc));
    assert_eq!(zsh.matches(".tokenrein/copilot.sh").count(), 2, "{zsh}");
    // The telemetry settings reach the copilot command, and only it.
    home.write(
        "bin/copilot",
        "#!/bin/sh\necho \"$OTEL_EXPORTER_OTLP_ENDPOINT|$OTEL_EXPORTER_OTLP_PROTOCOL|$OTEL_EXPORTER_OTLP_HEADERS\"\n",
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let exe = std::fs::Permissions::from_mode(0o755);
        std::fs::set_permissions(home.path("bin/copilot"), exe).unwrap();
    }
    let sh = format!(
        "PATH={}:$PATH; . \"$HOME/.tokenrein/copilot.sh\"; copilot; echo \"[$OTEL_EXPORTER_OTLP_ENDPOINT]\"",
        home.path("bin").display()
    );
    let (code, out, err) = run(&home, "sh", &["-c", &sh], "").await;
    assert_eq!(code, 0, "{err}");
    assert_eq!(
        out,
        format!(
            "{}/v1/otel|http/json|Authorization=Bearer {KEY},x-tr-agent=copilot\n[]\n",
            h.proxy
        )
    );

    let cmd = doc["hooks"]["preToolUse"][0]["bash"]
        .as_str()
        .unwrap()
        .to_string();
    // Copilot's native payload, which doesn't name its event.
    let event = json!({"sessionId": "cs-9", "timestamp": 1792641600000_i64, "cwd": "/repo",
                       "toolName": "bash", "toolArgs": {"command": "ls"}});
    assert_eq!(run_hook(&home, &cmd, event.clone()).await, json!({}));
    h.pause("copilot-cs-9").await;
    let answer = run_hook(&home, &cmd, event).await;
    assert_eq!(answer["permissionDecision"], "deny");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        for f in [".tokenrein/copilot.json", ".tokenrein/copilot.sh"] {
            let mode = std::fs::metadata(home.path(f))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o077, 0, "{f}");
        }
    }

    let (code, _, err) = run(&home, "python3", &["-I", &script, "--remove"], "").await;
    assert_eq!(code, 0, "{err}");
    assert_eq!(home.read(".zshrc"), zshrc);
    assert!(!home.path(".copilot/hooks/tokenrein.json").exists());
    assert!(!home.path(".tokenrein/copilot.sh").exists());
    assert!(!home.path(".tokenrein/hook.py").exists());

    // Someone else's file of the same name is left alone.
    let theirs = "{\"version\": 1, \"hooks\": {}}";
    home.write(".copilot/hooks/tokenrein.json", theirs);
    let (code, _, err) = run(&home, "python3", &args, "").await;
    assert_eq!(code, 1);
    assert!(err.contains("isn't Tokenrein's"), "{err}");
    assert_eq!(home.read(".copilot/hooks/tokenrein.json"), theirs);
}

#[tokio::test]
async fn copilot_policy_setup() {
    let Some(h) = H::start().await else { return };
    if !have_python().await {
        return;
    }
    let home = Home::new();
    let root = home.path("root").to_string_lossy().into_owned();
    let managed = if cfg!(target_os = "macos") {
        "root/Library/Application Support/GitHubCopilot/managed-settings.json"
    } else {
        "root/etc/github-copilot/managed-settings.json"
    };
    let policy_hooks = "root/etc/github-copilot/policy.d/tokenrein.json";
    home.write(managed, "{\"model\": \"auto\"}");
    let script = h.script(&home, "copilot").await;
    let base = ["-I", &script, "--policy", "--policy-root", &root];
    let mut args = base.to_vec();
    args.extend(["--key", KEY, "--url", &h.proxy]);
    let (code, out, err) = run(&home, "python3", &args, "").await;
    assert_eq!(code, 0, "{out}{err}");

    let settings: Value = serde_json::from_str(&home.read(managed)).unwrap();
    assert_eq!(settings["model"], "auto");
    let t = &settings["telemetry"];
    assert_eq!(t["enabled"], true);
    assert_eq!(t["endpoint"], format!("{}/v1/otel", h.proxy));
    assert_eq!(t["protocol"], "http/json");
    assert_eq!(t["headers"]["Authorization"], format!("Bearer {KEY}"));
    assert_eq!(t["capture"], json!({"identity": true}));
    let doc: Value = serde_json::from_str(&home.read(policy_hooks)).unwrap();
    let cmd = doc["hooks"]["preToolUse"][0]["bash"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(cmd.contains("etc/tokenrein/hook.py"), "{cmd}");
    // Nothing in the user's own home.
    assert!(!home.path(".tokenrein").exists());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        for f in ["root/etc/tokenrein/copilot.json", policy_hooks, managed] {
            let mode = std::fs::metadata(home.path(f))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o644, "{f}");
        }
    }
    let event = json!({"sessionId": "cs-pol", "cwd": "/repo", "toolName": "edit", "toolArgs": {}});
    assert_eq!(run_hook(&home, &cmd, event.clone()).await, json!({}));
    h.pause("copilot-cs-pol").await;
    assert_eq!(
        run_hook(&home, &cmd, event).await["permissionDecision"],
        "deny"
    );

    let mut undo = base.to_vec();
    undo.push("--remove");
    let (code, _, err) = run(&home, "python3", &undo, "").await;
    assert_eq!(code, 0, "{err}");
    let settings: Value = serde_json::from_str(&home.read(managed)).unwrap();
    assert_eq!(settings, json!({"model": "auto"}));
    assert!(!home.path(policy_hooks).exists());
    assert!(!home.path("root/etc/tokenrein/hook.py").exists());

    // Another telemetry destination is left alone.
    let foreign =
        "{\"telemetry\": {\"enabled\": true, \"endpoint\": \"https://otel.example.com\"}}";
    home.write(managed, foreign);
    let (code, _, err) = run(&home, "python3", &args, "").await;
    assert_eq!(code, 1);
    assert!(err.contains("telemetry"), "{err}");
    assert_eq!(home.read(managed), foreign);
}
