//! Ask-mode approvals, loop detection and alerts, against a real redis-server, a fake OpenAI
//! upstream, and fake Slack / webhook / PagerDuty receivers.

mod common;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::State;
use axum::http::HeaderMap;
use axum::routing::post;
use axum::{Json, Router};
use reqwest::{Method, StatusCode};
use serde_json::{Value, json};
use tokenrein::config::{Resolved, sha256_hex};
use tokenrein::{AppState, admin, alerts, build_state, control, proxy};

const KEY: &str = "tr_guards_test_key_0001";
const ADMIN_TOKEN: &str = "admin-token-0123456789";
const SECRETS_KEY: &str = "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f";

async fn fake_openai() -> Json<Value> {
    Json(json!({
        "id": "c1", "object": "chat.completion", "model": "gpt-4o-mini",
        "choices": [{"index": 0, "message": {"role": "assistant", "content": "ok"}, "finish_reason": "stop"}],
        "usage": {"prompt_tokens": 1000, "completion_tokens": 500}
    }))
}

/// What the fake alert receivers got: (path, headers, JSON body, raw body).
type Received = (String, HashMap<String, String>, Value, Vec<u8>);
type Inbox = Arc<Mutex<Vec<Received>>>;

async fn receive(
    State(inbox): State<Inbox>,
    uri: axum::http::Uri,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> &'static str {
    let h = headers
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or("").to_string()))
        .collect();
    let json = serde_json::from_slice(&body).unwrap_or(Value::Null);
    inbox
        .lock()
        .unwrap()
        .push((uri.path().to_string(), h, json, body.to_vec()));
    "ok"
}

async fn serve(router: Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    format!("http://{addr}")
}

struct H {
    _redis: common::RedisServer,
    state: Arc<AppState>,
    proxy: String,
    admin: String,
    inbox: Inbox,
    receiver: String,
    http: reqwest::Client,
}

impl H {
    /// `budgets` is TOML for the project's `[orgs.projects.budgets]` table.
    async fn start(budgets: &str) -> Option<Self> {
        let redis = common::RedisServer::start()?;
        let upstream = serve(Router::new().route("/v1/chat/completions", post(fake_openai))).await;
        let inbox = Inbox::default();
        let receiver = serve(
            Router::new()
                .fallback(post(receive))
                .with_state(inbox.clone()),
        )
        .await;
        let toml = format!(
            r#"
            [server]
            listen = "127.0.0.1:0"
            admin_token_env = "ADMIN"
            secrets_key_env = "SECRETS"
            [redis]
            url = "{redis}"
            [auth]
            public_url = "https://tokenrein.example.com"
            [providers.openai]
            base_url = "{upstream}"
            api_key_env = "OPENAI_KEY"
            [[orgs]]
            id = "acme"
            [[orgs.projects]]
            id = "bot"
            virtual_key_hashes = ["{hash}"]
            [orgs.projects.budgets]
            {budgets}
            "#,
            redis = redis.url,
            hash = sha256_hex(KEY),
        );
        let pd = format!("{receiver}/pagerduty");
        let rt = Resolved::from_toml(&toml, move |name| match name {
            "OPENAI_KEY" => Some("sk-test".into()),
            "ADMIN" => Some(ADMIN_TOKEN.into()),
            "SECRETS" => Some(SECRETS_KEY.into()),
            "TOKENREIN_PAGERDUTY_URL" => Some(pd.clone()),
            other => common::licensed(other),
        })
        .unwrap();
        let state = build_state(rt).await.unwrap();
        control::init(&state).await.unwrap();
        let proxy = serve(proxy::router(state.clone())).await;
        let admin = serve(admin::router(state.clone())).await;
        Some(Self {
            _redis: redis,
            state,
            proxy,
            admin,
            inbox,
            receiver,
            http: reqwest::Client::new(),
        })
    }

    async fn chat(&self, run: &str, text: &str) -> (StatusCode, Value) {
        let resp = self
            .http
            .post(format!("{}/v1/chat/completions", self.proxy))
            .bearer_auth(KEY)
            .header("x-tr-run-id", run)
            .header("x-tr-agent", "writer")
            .json(&json!({"model": "gpt-4o-mini", "max_tokens": 100,
                          "messages": [{"role": "user", "content": text}]}))
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

    async fn ingest(&self, path: &str, body: Value) -> (StatusCode, Value) {
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

    async fn approvals(&self) -> Value {
        self.admin(Method::GET, "/admin/orgs/acme/approvals", None)
            .await
            .1
    }

    /// Wait for at least `n` alerts (they are sent in the background).
    async fn alerts(&self, n: usize) -> Vec<Received> {
        for _ in 0..100 {
            if self.inbox.lock().unwrap().len() >= n {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        self.inbox.lock().unwrap().clone()
    }

    async fn add_channel(&self, kind: &str, target: &str, events: Value) -> Value {
        let body = json!({"kind": kind, "name": format!("{kind} channel"), "target": target, "events": events});
        let (status, created) = self
            .admin(Method::POST, "/admin/orgs/acme/alerts", Some(body))
            .await;
        assert_eq!(status, StatusCode::CREATED, "{created}");
        created
    }
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
async fn ask_mode_pauses_the_run_until_an_approver_decides() {
    // Each call costs about $0.00045 and reserves $0.00006 up front: over this budget at once.
    let Some(h) = H::start(r#"run = { limit_usd = 0.00001, mode = "ask" }"#).await else {
        return;
    };
    let (status, body) = h.chat("job-1", "hi").await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(body["error"]["type"], "tokenrein_run_paused");
    let message = body["error"]["message"].as_str().unwrap();
    assert!(message.contains("waiting for an approver"), "{message}");
    // Later calls wait too, without another request.
    assert_eq!(h.chat("job-1", "again").await.0, StatusCode::FORBIDDEN);
    let list = h.approvals().await;
    assert_eq!(list["pending"], 1);
    let a = &list["approvals"][0];
    assert_eq!(a["run_id"], "job-1");
    assert_eq!(a["kind"], "budget");
    assert_eq!(a["status"], "pending");
    assert_eq!(a["scope"], "run:job-1");

    // Approving with more budget lets the run continue.
    let more = Some(json!({"add_usd": 1.0}));
    let approve = "/admin/orgs/acme/approvals/job-1/approve";
    let (status, body) = h.admin(Method::POST, approve, more).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["state"], "running");
    assert_eq!(h.chat("job-1", "now").await.0, StatusCode::OK);
    let (status, _) = h.admin(Method::POST, approve, None).await;
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "decided requests stay decided"
    );

    // Denying stops the run for good.
    assert_eq!(h.chat("job-2", "hi").await.0, StatusCode::FORBIDDEN);
    let deny = "/admin/orgs/acme/approvals/job-2/deny";
    assert_eq!(h.admin(Method::POST, deny, None).await.0, StatusCode::OK);
    let (_, body) = h.chat("job-2", "please").await;
    assert_eq!(body["error"]["type"], "tokenrein_run_killed");

    let (_, log) = h.admin(Method::GET, "/admin/audit?limit=10", None).await;
    let seen = actions(&log);
    for a in ["approval.requested", "approval.approve", "approval.deny"] {
        assert!(seen.contains(&a), "{a} missing from {seen:?}");
    }
}

#[tokio::test]
async fn ask_mode_is_only_for_run_budgets() {
    let Some(h) = H::start("").await else { return };
    let body = json!({"budgets": {"agent_daily": {"limit_usd": 5, "mode": "ask"}}});
    let (status, err) = h
        .admin(Method::PUT, "/admin/orgs/acme/projects/bot", Some(body))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(err["error"].as_str().unwrap().contains("ask"), "{err}");
}

#[tokio::test]
async fn repeated_requests_pause_a_looping_run() {
    let Some(h) = H::start(r#"loops = { action = "pause", max_repeats = 3 }"#).await else {
        return;
    };
    assert_eq!(h.chat("loopy", "same thing").await.0, StatusCode::OK);
    assert_eq!(h.chat("loopy", "same thing").await.0, StatusCode::OK);
    // The third identical request trips the guard and is not sent.
    let (status, body) = h.chat("loopy", "same thing").await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert!(body["error"]["message"].as_str().unwrap().contains("loop"));
    let (status, _) = h.chat("loopy", "something new").await;
    assert_eq!(status, StatusCode::FORBIDDEN, "the run is paused");
    let a = &h.approvals().await["approvals"][0];
    assert_eq!(a["kind"], "loop");
    assert_eq!(a["run_id"], "loopy");
    let detail = a["detail"].as_str().unwrap();
    assert!(detail.contains("same request 3 times"), "{detail}");
    // Other runs are not affected.
    assert_eq!(h.chat("other", "same thing").await.0, StatusCode::OK);
    // An approver lets it go on.
    let approve = "/admin/orgs/acme/approvals/loopy/approve";
    assert_eq!(h.admin(Method::POST, approve, None).await.0, StatusCode::OK);
    assert_eq!(h.chat("loopy", "something new").await.0, StatusCode::OK);
}

#[tokio::test]
async fn retries_refused_during_a_budget_pause_are_not_a_loop() {
    let budgets = r#"run = { limit_usd = 0.00001, mode = "ask" }
                     loops = { action = "pause", max_repeats = 3 }"#;
    let Some(h) = H::start(budgets).await else {
        return;
    };
    // The agent keeps retrying the same request while it waits for a person.
    for _ in 0..6 {
        let (status, body) = h.chat("patient", "same thing").await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
        assert_eq!(body["error"]["type"], "tokenrein_run_paused");
    }
    let list = h.approvals().await;
    assert_eq!(list["pending"], 1);
    assert_eq!(list["approvals"][0]["kind"], "budget");

    // The approver adds budget, and the retried request goes through.
    let more = Some(json!({"add_usd": 1.0}));
    let approve = "/admin/orgs/acme/approvals/patient/approve";
    assert_eq!(h.admin(Method::POST, approve, more).await.0, StatusCode::OK);
    let (status, body) = h.chat("patient", "same thing").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(h.approvals().await["pending"], 0);
}

#[tokio::test]
async fn checks_and_hooks_refused_while_paused_are_not_a_loop() {
    let Some(h) = H::start(r#"loops = { action = "pause", max_repeats = 3 }"#).await else {
        return;
    };
    let fingerprint = "00ff00ff00ff00ff00ff00ff00ff00ff";
    let check = json!({"run_id": "sdk-run", "model": "gpt-4o-mini", "fingerprint": fingerprint});
    let hook = json!({"hook_event_name": "PreToolUse", "session_id": "s1", "tool_name": "Bash",
                      "tool_input": {"command": "npm test"}});
    for run in ["sdk-run", "cc-s1"] {
        let pause = format!("/admin/orgs/acme/runs/{run}/pause");
        assert_eq!(h.admin(Method::POST, &pause, None).await.0, StatusCode::OK);
    }
    for _ in 0..6 {
        let (_, v) = h.ingest("/v1/check", check.clone()).await;
        assert_eq!(v["allowed"], false, "{v}");
        let (_, v) = h.ingest("/v1/hooks/claude-code", hook.clone()).await;
        let decision = &v["hookSpecificOutput"]["permissionDecision"];
        assert_eq!(decision, "deny", "{v}");
    }
    for run in ["sdk-run", "cc-s1"] {
        let resume = format!("/admin/orgs/acme/runs/{run}/resume");
        assert_eq!(h.admin(Method::POST, &resume, None).await.0, StatusCode::OK);
    }
    let (_, v) = h.ingest("/v1/check", check).await;
    assert_eq!(v["allowed"], true, "{v}");
    let (_, v) = h.ingest("/v1/hooks/claude-code", hook).await;
    assert_eq!(v, json!({}));
    assert_eq!(h.approvals().await["pending"], 0);
}

#[tokio::test]
async fn a_call_stopped_by_the_loop_guard_keeps_nothing_reserved() {
    let budgets = r#"run = { limit_usd = 1.0, mode = "enforce" }
                     loops = { action = "pause", max_repeats = 2 }"#;
    let Some(h) = H::start(budgets).await else {
        return;
    };
    assert_eq!(h.chat("held", "same thing").await.0, StatusCode::OK);
    assert_eq!(h.chat("held", "same thing").await.0, StatusCode::FORBIDDEN);
    let (_, run) = h
        .admin(Method::GET, "/admin/orgs/acme/runs/held", None)
        .await;
    assert_eq!(run["budget"]["reserved_micros"], 0, "{run}");
}

#[tokio::test]
async fn letting_a_looping_run_continue_resets_the_guard() {
    let Some(h) = H::start(r#"loops = { action = "pause", max_repeats = 3 }"#).await else {
        return;
    };
    assert_eq!(h.chat("loopy", "same thing").await.0, StatusCode::OK);
    assert_eq!(h.chat("loopy", "same thing").await.0, StatusCode::OK);
    assert_eq!(h.chat("loopy", "same thing").await.0, StatusCode::FORBIDDEN);
    let approve = "/admin/orgs/acme/approvals/loopy/approve";
    assert_eq!(h.admin(Method::POST, approve, None).await.0, StatusCode::OK);
    // The count starts over: the same request gets the same allowance again, then trips again.
    assert_eq!(h.chat("loopy", "same thing").await.0, StatusCode::OK);
    assert_eq!(h.chat("loopy", "same thing").await.0, StatusCode::OK);
    assert_eq!(h.chat("loopy", "same thing").await.0, StatusCode::FORBIDDEN);
    assert_eq!(h.approvals().await["pending"], 1);
}

#[tokio::test]
async fn sdk_checks_and_agent_hooks_catch_loops_too() {
    let policy = r#"loops = { action = "kill", max_repeats = 2, max_usd_per_minute = 0.01 }"#;
    let Some(h) = H::start(policy).await else {
        return;
    };
    // SDK direct mode: the SDK sends a hash of the request, never its content.
    let check = json!({"run_id": "sdk-run", "model": "gpt-4o-mini",
                       "fingerprint": "00ff00ff00ff00ff00ff00ff00ff00ff"});
    let (_, v) = h.ingest("/v1/check", check.clone()).await;
    assert_eq!(v["allowed"], true, "{v}");
    let (_, v) = h.ingest("/v1/check", check).await;
    assert_eq!(v["allowed"], false, "{v}");
    assert_eq!(v["run_state"], "killed");

    // Claude Code: the same tool call over and over.
    let hook = json!({"hook_event_name": "PreToolUse", "session_id": "s1", "tool_name": "Bash",
                      "tool_input": {"command": "npm test"}});
    let (_, first) = h.ingest("/v1/hooks/claude-code", hook.clone()).await;
    assert_eq!(first, json!({}));
    let (_, second) = h.ingest("/v1/hooks/claude-code", hook).await;
    let decision = &second["hookSpecificOutput"]["permissionDecision"];
    assert_eq!(decision, "deny", "{second}");

    // Spend velocity, from reported usage: one call over $0.01 within a minute.
    let usage = json!({"events": [{"run_id": "fast", "model": "gpt-4o-mini", "cost_usd": 0.02}]});
    assert_eq!(h.ingest("/v1/usage", usage).await.0, StatusCode::OK);
    let (_, v) = h.ingest("/v1/check", json!({"run_id": "fast"})).await;
    assert_eq!(v["run_state"], "killed", "{v}");
}

#[tokio::test]
async fn alerts_reach_slack_webhooks_and_pagerduty() {
    let budgets = r#"project_monthly = { limit_usd = 0.0006, mode = "enforce" }
                     loops = { action = "warn", max_repeats = 2 }"#;
    let Some(h) = H::start(budgets).await else {
        return;
    };
    let slack_url = format!("{}/slack", h.receiver);
    let slack = h
        .add_channel("slack", &slack_url, json!(["budget_blocked"]))
        .await;
    assert_eq!(slack["signing_secret"], Value::Null);
    let hook_url = format!("{}/hook", h.receiver);
    let hook = h.add_channel("webhook", &hook_url, json!([])).await;
    let secret = hook["signing_secret"].as_str().unwrap().to_string();
    assert!(secret.starts_with("whsec_"));
    h.add_channel(
        "pagerduty",
        "R0UT1NGKEY0123456789abcd",
        json!(["loop_detected"]),
    )
    .await;
    // Targets and secrets are never listed back.
    let (_, list) = h.admin(Method::GET, "/admin/orgs/acme/alerts", None).await;
    let text = list.to_string();
    assert!(!text.contains(&h.receiver), "{text}");
    assert!(
        !text.contains("R0UT1NG") && !text.contains(&secret),
        "{text}"
    );
    assert_eq!(list["channels"].as_array().unwrap().len(), 3);

    // A loop in warn mode only alerts (webhook + PagerDuty); the run keeps going.
    assert_eq!(h.chat("r1", "same").await.0, StatusCode::OK);
    assert_eq!(h.chat("r1", "same").await.0, StatusCode::OK);
    // The project budget is now used up: refused, and Slack + webhook hear about it.
    assert_eq!(h.chat("r2", "more").await.0, StatusCode::PAYMENT_REQUIRED);
    let got = h.alerts(4).await;
    let to = |p: &str| got.iter().filter(|a| a.0 == p).collect::<Vec<_>>();

    let pd = to("/pagerduty");
    assert_eq!(pd.len(), 1, "{got:?}");
    assert_eq!(pd[0].2["routing_key"], "R0UT1NGKEY0123456789abcd");
    assert_eq!(pd[0].2["payload"]["group"], "loop_detected");
    let link = &pd[0].2["links"][0]["href"];
    assert_eq!(link, "https://tokenrein.example.com/#tab=runs");

    let sl = to("/slack");
    assert_eq!(sl.len(), 1, "Slack only wants budget_blocked: {got:?}");
    assert!(sl[0].2["text"].as_str().unwrap().contains("Budget used up"));

    let wh = to("/hook");
    let events: Vec<&str> = wh.iter().map(|a| a.2["event"].as_str().unwrap()).collect();
    assert_eq!(events, ["loop_detected", "budget_blocked"], "{got:?}");
    // Webhooks are signed: HMAC-SHA256 of "<timestamp>.<body>".
    for (_, headers, _, raw) in wh {
        let ts = &headers["x-tokenrein-timestamp"];
        assert_eq!(
            headers["x-tokenrein-signature"],
            alerts::sign(&secret, ts, raw)
        );
    }

    // The same refusal within its quiet period is not sent again.
    let before = h.alerts(0).await.len();
    assert_eq!(h.chat("r3", "more").await.0, StatusCode::PAYMENT_REQUIRED);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(h.alerts(0).await.len(), before);

    // Delivery results are shown per channel; the test button sends one now.
    let (_, list) = h.admin(Method::GET, "/admin/orgs/acme/alerts", None).await;
    assert_eq!(list["channels"][1]["last_delivery"]["ok"], true, "{list}");
    let id = list["channels"][0]["id"].as_str().unwrap().to_string();
    let test = format!("/admin/orgs/acme/alerts/{id}/test");
    let (_, t) = h.admin(Method::POST, &test, None).await;
    assert_eq!(t["ok"], true, "{t}");
    let remove = format!("/admin/orgs/acme/alerts/{id}");
    assert_eq!(
        h.admin(Method::DELETE, &remove, None).await.0,
        StatusCode::NO_CONTENT
    );
}

#[tokio::test]
async fn budgets_past_80_percent_warn_once() {
    let budgets = r#"project_monthly = { limit_usd = 0.001, mode = "check" }"#;
    let Some(h) = H::start(budgets).await else {
        return;
    };
    let hook_url = format!("{}/hook", h.receiver);
    h.add_channel("webhook", &hook_url, json!(["budget_warning"]))
        .await;
    // Two calls of about $0.00045 each: 90% of $0.001.
    assert_eq!(h.chat("w1", "a").await.0, StatusCode::OK);
    assert_eq!(h.chat("w1", "b").await.0, StatusCode::OK);
    alerts::watch_budgets(&h.state).await.unwrap();
    alerts::watch_budgets(&h.state).await.unwrap();
    let got = h.alerts(1).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(h.alerts(0).await.len(), 1, "warned once: {got:?}");
    assert_eq!(got[0].2["event"], "budget_warning");
    let title = got[0].2["title"].as_str().unwrap();
    assert!(title.contains("project bot monthly"), "{title}");
}
