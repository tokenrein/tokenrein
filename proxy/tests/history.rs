//! Usage history: events reported to the proxy end up in Parquet files and come back through
//! the spend report (ranges past Redis's 40 days), the CSV export and old run timelines.

mod common;

use std::sync::Arc;

use reqwest::StatusCode;
use serde_json::{Value, json};
use tokenrein::config::{Resolved, sha256_hex};
use tokenrein::{AppState, admin, build_state, control, history, proxy};

const KEY: &str = "tr_history_test_key_0001";
const ADMIN_TOKEN: &str = "admin-token-0123456789";

struct H {
    _redis: common::RedisServer,
    state: Arc<AppState>,
    proxy: String,
    admin: String,
    dir: std::path::PathBuf,
    http: reqwest::Client,
}

async fn serve(router: axum::Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    format!("http://{addr}")
}

impl H {
    async fn start(with_history: bool) -> Option<Self> {
        Self::start_with(with_history, "", false).await
    }

    /// `extra` goes into the config before the orgs; `licensed` adds an Enterprise license.
    async fn start_with(with_history: bool, extra: &str, licensed: bool) -> Option<Self> {
        let redis = common::RedisServer::start()?;
        let dir = std::env::temp_dir().join(format!(
            "tokenrein-history-it-{}",
            uuid::Uuid::new_v4().simple()
        ));
        let section = if with_history {
            format!(
                "[history]\ndir = {:?}\nflush_secs = 3600",
                dir.to_string_lossy()
            )
        } else {
            String::new()
        };
        let toml = format!(
            r#"
            [server]
            listen = "127.0.0.1:0"
            admin_token_env = "ADMIN"
            [redis]
            url = "{redis}"
            {section}
            {extra}
            [[orgs]]
            id = "acme"
            [[orgs.projects]]
            id = "bot"
            virtual_key_hashes = ["{hash}"]
            "#,
            redis = redis.url,
            hash = sha256_hex(KEY),
        );
        let rt = Resolved::from_toml(&toml, move |name| match name {
            "ADMIN" => Some(ADMIN_TOKEN.into()),
            "TOKENREIN_ANALYSIS_API_KEY" => Some("sk-analysis".into()),
            other if licensed => common::licensed(other),
            _ => None,
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
            dir,
            http: reqwest::Client::new(),
        })
    }

    async fn report(&self, events: Value) {
        let resp = self
            .http
            .post(format!("{}/v1/usage", self.proxy))
            .bearer_auth(KEY)
            .json(&json!({ "events": events }))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    async fn get(&self, path: &str) -> (StatusCode, String) {
        let resp = self
            .http
            .get(format!("{}{path}", self.admin))
            .bearer_auth(ADMIN_TOKEN)
            .send()
            .await
            .unwrap();
        (resp.status(), resp.text().await.unwrap())
    }

    /// A GET that must succeed, as JSON.
    async fn json(&self, path: &str) -> Value {
        let (status, body) = self.get(path).await;
        assert_eq!(status, StatusCode::OK, "{path}: {body}");
        serde_json::from_str(&body).unwrap()
    }

    /// Wait for the activity writer to hand the events over, then write them.
    async fn flush(&self, rows: usize) {
        let h = self.state.history.clone().unwrap();
        let mut written = 0;
        for _ in 0..50 {
            written += h.flush().await.unwrap();
            if written >= rows {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        panic!("only {written} of {rows} history rows written");
    }

    /// Like [`flush`](Self::flush), for the step history.
    async fn flush_steps(&self, rows: usize) {
        let h = self.state.step_history.clone().unwrap();
        let mut written = 0;
        for _ in 0..50 {
            written += h.flush().await.unwrap();
            if written >= rows {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        panic!("only {written} of {rows} step rows written");
    }

    async fn steps(&self, steps: Value) {
        let resp = self
            .http
            .post(format!("{}/v1/steps", self.proxy))
            .bearer_auth(KEY)
            .json(&json!({ "steps": steps }))
            .send()
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::OK,
            "{}",
            resp.text().await.unwrap()
        );
    }
}

impl Drop for H {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

#[tokio::test]
async fn reported_usage_comes_back_from_the_history() {
    let Some(h) = H::start(true).await else {
        return;
    };
    h.report(json!([
        {"run_id": "r1", "agent": "writer", "user": "ann@example.com", "provider": "openai",
         "model": "gpt-4o-mini", "input_tokens": 1000, "output_tokens": 100, "cost_usd": 1.25},
        {"run_id": "r1", "agent": "writer", "user": "ann@example.com", "provider": "openai",
         "model": "gpt-4o-mini", "input_tokens": 500, "output_tokens": 50, "cost_usd": 0.75},
        {"run_id": "r2", "agent": "reviewer", "provider": "anthropic",
         "model": "claude-sonnet-4-5", "input_tokens": 10, "output_tokens": 10, "cost_usd": 0.5},
    ]))
    .await;
    h.flush(3).await;

    // 90 days is past what Redis keeps, so the report comes from the history.
    let (status, body) = h.get("/admin/orgs/acme/spend?days=90").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let s: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(s["source"], "history");
    assert_eq!(s["days"].as_array().unwrap().len(), 90);
    let today = s["days"][89].clone();
    assert_eq!(today["cost_micros"], 2_500_000);
    assert_eq!(today["requests"], 3);
    assert_eq!(s["by"]["user"][0]["key"], "ann@example.com");
    assert_eq!(s["by"]["user"][0]["cost_micros"], 2_000_000);
    assert_eq!(s["by"]["agent"][0]["key"], "bot/writer");

    // 30 days still comes from Redis, and both agree.
    let (_, body) = h.get("/admin/orgs/acme/spend?days=30").await;
    let r: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(r["source"], "redis");
    assert_eq!(r["history_days"], 400);
    assert_eq!(r["days"][29]["cost_micros"], 2_500_000);

    let (status, csv) = h.get("/admin/orgs/acme/spend?days=7&format=csv").await;
    assert_eq!(status, StatusCode::OK);
    assert!(csv.starts_with("day,project,agent,user,provider,model,requests,blocked,cost_usd\n"));
    assert!(
        csv.contains(",bot,writer,ann@example.com,openai,gpt-4o-mini,2,0,2.000000"),
        "{csv}"
    );

    // A run Redis no longer has: rebuilt from the history, with its sub-agent.
    let history = h.state.history.clone().unwrap();
    let old = (chrono::Utc::now() - chrono::Duration::days(20)).to_rfc3339();
    history.push(&[
        json!({"org": "acme", "project": "bot", "agent": "planner", "run_id": "old-run",
               "ts": old, "decision": "allowed", "model": "gpt-4o", "provider": "openai",
               "actual_micros": 400_000}),
        json!({"org": "acme", "project": "bot", "agent": "coder", "run_id": "old-run-sub",
               "parent_run_id": "old-run", "ts": old, "decision": "blocked_budget",
               "model": "gpt-4o", "provider": "openai", "actual_micros": 0}),
    ]);
    history.flush().await.unwrap();
    let (status, body) = h.get("/admin/orgs/acme/runs/old-run/timeline").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let t: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(t["source"], "history");
    assert_eq!(t["events"].as_array().unwrap().len(), 2);
    let runs = t["runs"].as_array().unwrap();
    assert_eq!(runs.len(), 2);
    assert!(
        runs.iter()
            .any(|r| r["run_id"] == "old-run-sub" && r["blocked"] == 1)
    );

    // Days with a single file have nothing to merge; totals cover both days.
    assert_eq!(history::maintain(&h.state, &history).await.unwrap(), 0);
    let (_, body) = h.get("/admin/orgs/acme/spend?days=60").await;
    let s: Value = serde_json::from_str(&body).unwrap();
    let total: i64 = s["days"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["cost_micros"].as_i64().unwrap())
        .sum();
    assert_eq!(total, 2_900_000);

    let (_, metrics) = h.get("/metrics").await;
    assert!(
        metrics.contains("tokenrein_history_rows_total{result=\"written\"} "),
        "{metrics}"
    );
}

#[tokio::test]
async fn without_a_history_spend_stays_within_redis() {
    let Some(h) = H::start(false).await else {
        return;
    };
    assert!(h.state.history.is_none());
    let (status, body) = h.get("/admin/orgs/acme/spend?days=365").await;
    assert_eq!(status, StatusCode::OK);
    let s: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(s["source"], "redis");
    assert_eq!(s["history_days"], Value::Null);
    assert_eq!(s["days"].as_array().unwrap().len(), 40);
    let (status, csv) = h.get("/admin/orgs/acme/spend?days=7&format=csv").await;
    assert_eq!(status, StatusCode::OK);
    assert!(csv.starts_with("day,requests,blocked,cost_usd\n"));
    assert_eq!(csv.lines().count(), 8);
}

/// Sum of `cost_micros` over a range report's buckets.
fn bucket_cost(s: &Value) -> i64 {
    s["buckets"]
        .as_array()
        .unwrap()
        .iter()
        .map(|b| b["cost_micros"].as_i64().unwrap())
        .sum()
}

#[tokio::test]
async fn spend_ranges_have_minute_and_hour_buckets_and_filters() {
    let Some(h) = H::start(true).await else {
        return;
    };
    h.report(json!([
        {"run_id": "r1", "agent": "writer", "user": "ann@example.com", "provider": "openai",
         "model": "gpt-4o-mini", "input_tokens": 1000, "output_tokens": 100, "cost_usd": 1.25},
        {"run_id": "r1", "agent": "writer", "user": "ann@example.com", "provider": "openai",
         "model": "gpt-4o-mini", "input_tokens": 500, "output_tokens": 50, "cost_usd": 0.75},
        {"run_id": "r2", "agent": "reviewer", "provider": "anthropic",
         "model": "claude-sonnet-4-5", "input_tokens": 10, "output_tokens": 10, "cost_usd": 0.5},
    ]))
    .await;
    let from = (chrono::Utc::now() - chrono::Duration::minutes(10)).to_rfc3339();
    let from = from.replace('+', "%2B");
    let range = |extra: &str| format!("/admin/orgs/acme/spend?from={from}{extra}");

    // Nothing is flushed yet (flush_secs is an hour): the report reads the rows still in memory.
    let mut s = Value::Null;
    for _ in 0..50 {
        let (status, body) = h.get(&range("")).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        s = serde_json::from_str(&body).unwrap();
        if bucket_cost(&s) == 2_500_000 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert_eq!(s["source"], "history");
    assert_eq!(s["bucket"], "minute", "ten minutes is shown by the minute");
    assert_eq!(s["bucket_secs"], 60);
    let n = s["buckets"].as_array().unwrap().len();
    assert!((10..=11).contains(&n), "{n} buckets");
    assert_eq!(bucket_cost(&s), 2_500_000);
    assert_eq!(s["options"]["user"], json!(["ann@example.com", "-"]));

    // Filters narrow the totals and the breakdowns, not the filter menus.
    let ann = h.json(&range("&user=ann%40example.com")).await;
    assert_eq!(bucket_cost(&ann), 2_000_000);
    assert_eq!(ann["by"]["model"].as_array().unwrap().len(), 1);
    assert_eq!(ann["options"]["user"].as_array().unwrap().len(), 2);
    assert_eq!(ann["filters"]["user"], "ann@example.com");
    assert_eq!(bucket_cost(&h.json(&range("&user=-")).await), 500_000);
    assert_eq!(
        bucket_cost(&h.json(&range("&agent=bot%2Freviewer")).await),
        500_000
    );
    assert_eq!(
        bucket_cost(&h.json(&range("&agent=reviewer")).await),
        500_000
    );
    assert_eq!(
        bucket_cost(
            &h.json(&range("&project=bot&model=anthropic%2Fclaude-sonnet-4-5"))
                .await
        ),
        500_000
    );
    assert_eq!(bucket_cost(&h.json(&range("&status=blocked")).await), 0);
    assert_eq!(
        bucket_cost(&h.json(&range("&status=allowed")).await),
        2_500_000
    );

    // Once written to Parquet, the same totals (nothing counted twice).
    h.flush(3).await;
    assert_eq!(bucket_cost(&h.json(&range("")).await), 2_500_000);

    // Unfiltered days within 40 days come from Redis (it has requests from before the history was
    // on); filtered ones from the history. Both agree here.
    let week = (chrono::Utc::now() - chrono::Duration::days(6)).timestamp();
    let days = h
        .json(&format!("/admin/orgs/acme/spend?from={week}&bucket=day"))
        .await;
    assert_eq!(days["source"], "redis");
    assert_eq!(days["history"], true);
    assert_eq!(bucket_cost(&days), 2_500_000);
    let days = h
        .json(&format!(
            "/admin/orgs/acme/spend?from={week}&bucket=day&project=bot"
        ))
        .await;
    assert_eq!(days["source"], "history");
    assert_eq!(bucket_cost(&days), 2_500_000);

    // An older event lands in its hour.
    let history = h.state.history.clone().unwrap();
    let old = chrono::Utc::now() - chrono::Duration::hours(30);
    history.push(&[
        json!({"org": "acme", "project": "bot", "agent": "planner", "run_id": "old",
        "ts": old.to_rfc3339(), "decision": "allowed", "model": "gpt-4o", "provider": "openai",
        "actual_micros": 400_000}),
    ]);
    history.flush().await.unwrap();
    let three_days = (chrono::Utc::now() - chrono::Duration::days(3)).timestamp();
    let (_, body) = h
        .get(&format!(
            "/admin/orgs/acme/spend?from={three_days}&bucket=hour"
        ))
        .await;
    let s: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(s["bucket"], "hour");
    assert_eq!(bucket_cost(&s), 2_900_000);
    let hour = old.format("%Y-%m-%dT%H:00:00Z").to_string();
    let b = s["buckets"]
        .as_array()
        .unwrap()
        .iter()
        .find(|b| b["t"] == hour.as_str())
        .unwrap();
    assert_eq!(b["cost_micros"], 400_000);

    let (status, csv) = h
        .get(&format!(
            "/admin/orgs/acme/spend?from={three_days}&bucket=hour&user=ann%40example.com&format=csv"
        ))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert!(csv.starts_with("hour,project,agent,user,provider,model,requests,blocked,cost_usd\n"));
    assert!(
        csv.contains(",bot,writer,ann@example.com,openai,gpt-4o-mini,2,0,2.000000"),
        "{csv}"
    );
    assert_eq!(csv.lines().count(), 2, "only ann's row: {csv}");

    for bad in [
        format!("from={three_days}&bucket=minute"),
        "bucket=week".to_string(),
        format!("from={three_days}&to={}", three_days - 60),
        "status=maybe".to_string(),
        "from=yesterday".to_string(),
    ] {
        let (status, body) = h.get(&format!("/admin/orgs/acme/spend?{bad}")).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{bad}: {body}");
    }
}

#[tokio::test]
async fn without_a_history_ranges_are_daily_from_redis() {
    let Some(h) = H::start(false).await else {
        return;
    };
    h.report(
        json!([{"run_id": "r1", "agent": "writer", "provider": "openai",
        "model": "gpt-4o-mini", "input_tokens": 10, "output_tokens": 10, "cost_usd": 1.0}]),
    )
    .await;
    let week = (chrono::Utc::now() - chrono::Duration::days(6)).timestamp();
    let (status, body) = h
        .get(&format!("/admin/orgs/acme/spend?from={week}&bucket=day"))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let s: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(s["source"], "redis");
    assert_eq!(s["history"], false);
    assert_eq!(s["buckets"].as_array().unwrap().len(), 7);
    assert_eq!(s["options"]["model"], json!(["openai/gpt-4o-mini"]));
    for path in [
        "/admin/orgs/acme/spend?bucket=hour".to_string(),
        format!("/admin/orgs/acme/spend?from={week}&user=ann"),
    ] {
        let (status, body) = h.get(&path).await;
        assert_eq!(status, StatusCode::CONFLICT, "{path}: {body}");
        assert!(body.contains("usage history"), "{body}");
    }
}

#[tokio::test]
async fn insights_find_cache_misses_and_failing_tools() {
    let Some(h) = H::start(true).await else {
        return;
    };
    // Ten calls of one run re-send a 40k-token prompt with caching off.
    let calls: Vec<Value> = (0..10)
        .map(|_| {
            json!({"run_id": "loop-1", "agent": "coder", "provider": "anthropic",
            "model": "claude-opus-5-5", "input_tokens": 40_000, "output_tokens": 200})
        })
        .collect();
    h.report(json!(calls)).await;
    // A tool that fails most of the time, retried with the same input.
    let mut steps = Vec::new();
    for i in 0..10 {
        let input = if i < 3 {
            json!({"cmd": "make tset"})
        } else {
            json!({"cmd": format!("make test {i}")})
        };
        let id = format!("c{i}");
        steps.push(
            json!({"run_id": "loop-1", "agent": "coder", "kind": "tool_call", "name": "Bash",
            "call_id": id, "content": input}),
        );
        steps.push(json!({"run_id": "loop-1", "agent": "coder", "kind": "tool_result", "name": "Bash",
            "call_id": id, "status": if i < 6 { "error" } else { "ok" }, "content": "make: *** No rule"}));
    }
    h.steps(json!(steps)).await;
    h.flush(10).await;
    h.flush_steps(20).await;

    let r = h.json("/admin/orgs/acme/insights?days=1").await;
    let rules: Vec<&str> = r["findings"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["rule"].as_str().unwrap())
        .collect();
    for want in [
        "cache.not_cached",
        "tool.error_rate",
        "tool.retries_failing_input",
    ] {
        assert!(rules.contains(&want), "{want} missing from {rules:?}");
    }
    assert_eq!(r["cache"]["totals"]["calls"], 10);
    assert_eq!(r["cache"]["totals"]["hit_rate"], 0.0);
    assert!(r["cache"]["totals"]["missed_micros"].as_u64().unwrap() > 1_000_000);
    assert_eq!(r["tools"]["totals"]["calls"], 10);
    assert_eq!(r["tools"]["totals"]["errors"], 6);
    assert_eq!(r["days"], 1);
    // The weekly digest leads with the costliest finding.
    let digest = tokenrein::alerts::digest_alert(&r).unwrap();
    assert!(
        digest.title.ends_with("to fix in your agents this week"),
        "{}",
        digest.title
    );
    assert!(
        digest.text.contains("Prompt caching not turned on"),
        "{}",
        digest.text
    );
    assert!(
        digest
            .fields
            .iter()
            .any(|(k, v)| k == "Input from cache" && v == "0%")
    );
    assert!(tokenrein::alerts::digest_alert(&json!({"findings": []})).is_none());
    // Steps are stored as metadata only: the command text never reaches the files.
    let mut stored = String::new();
    for e in walk(&h.dir.join("steps")) {
        stored.push_str(&String::from_utf8_lossy(&std::fs::read(e).unwrap()));
    }
    assert!(!stored.is_empty() && !stored.contains("make tset"));
}

fn walk(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let p = e.path();
        if p.is_dir() {
            out.extend(walk(&p));
        } else {
            out.push(p);
        }
    }
    out
}

#[tokio::test]
async fn insights_need_the_history() {
    let Some(h) = H::start(false).await else {
        return;
    };
    let (status, body) = h.get("/admin/orgs/acme/insights").await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
}

/// What the fake endpoint was sent: the authorization header and the body.
type Seen = Arc<std::sync::Mutex<Vec<(String, Value)>>>;

/// A stand-in for an OpenAI-compatible endpoint such as LiteLLM: answers, reports a cost, and
/// keeps what it was sent.
async fn fake_analysis(
    axum::extract::State(seen): axum::extract::State<Seen>,
    headers: axum::http::HeaderMap,
    axum::Json(body): axum::Json<Value>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    let auth = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    seen.lock().unwrap().push((auth, body));
    (
        [("x-litellm-response-cost", "0.0042")],
        axum::Json(json!({
            "choices": [{"message": {"role": "assistant", "content": "- Return the valid targets in the error."}}],
            "usage": {"prompt_tokens": 900, "completion_tokens": 120},
        })),
    )
        .into_response()
}

#[tokio::test]
async fn suggestions_send_metrics_only_and_are_capped() {
    let seen: Seen = Arc::default();
    let endpoint = serve(
        axum::Router::new()
            .route("/v1/chat/completions", axum::routing::post(fake_analysis))
            .with_state(seen.clone()),
    )
    .await;
    let extra = format!(
        "[analysis]\nbase_url = \"{endpoint}/v1\"\nmodel = \"house-model\"\nmonthly_budget_usd = 0.006"
    );
    let Some(h) = H::start_with(true, &extra, true).await else {
        return;
    };
    let mut steps = Vec::new();
    for i in 0..10 {
        let id = format!("c{i}");
        steps.push(
            json!({"run_id": "r1", "agent": "coder", "kind": "tool_call", "name": "Deploy",
            "call_id": id, "content": {"target": "prod-secret-cluster"}}),
        );
        steps.push(json!({"run_id": "r1", "agent": "coder", "kind": "tool_result", "name": "Deploy",
            "call_id": id, "status": if i < 5 { "error" } else { "ok" }, "content": "no such target"}));
    }
    h.steps(json!(steps)).await;
    h.flush_steps(20).await;
    let r = h.json("/admin/orgs/acme/insights?days=1").await;
    assert_eq!(r["suggestions"]["available"], true, "{}", r["suggestions"]);
    assert_eq!(r["suggestions"]["model"], "house-model");

    let ask = |body: Value| {
        h.http
            .post(format!("{}/admin/orgs/acme/insights/suggest", h.admin))
            .bearer_auth(ADMIN_TOKEN)
            .json(&body)
            .send()
    };
    let resp = ask(json!({"rule": "tool.error_rate", "tool": "Deploy", "days": 1}))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let v: Value = resp.json().await.unwrap();
    assert!(v["suggestion"].as_str().unwrap().contains("valid targets"));
    assert_eq!(v["cost_micros"], 4_200);
    assert_eq!(v["sent"]["tool_metrics"]["calls"], 10);
    {
        let seen = seen.lock().unwrap();
        let (auth, body) = &seen[0];
        assert_eq!(auth, "Bearer sk-analysis");
        assert_eq!(body["model"], "house-model");
        let sent = body.to_string();
        assert!(sent.contains("tool.error_rate"));
        // Tool input and output never leave: only metrics do.
        assert!(
            !sent.contains("prod-secret-cluster") && !sent.contains("no such target"),
            "{sent}"
        );
    }
    // A finding that isn't in the report.
    let resp = ask(json!({"rule": "tool.error_rate", "tool": "Nope", "days": 1}))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    // The second answer passes the $0.006 monthly cap; the third is refused before calling.
    assert_eq!(
        ask(json!({"rule": "tool.error_rate", "tool": "Deploy", "days": 1}))
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );
    let resp = ask(json!({"rule": "tool.error_rate", "tool": "Deploy", "days": 1}))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(seen.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn suggestions_need_an_endpoint_and_a_license() {
    let Some(h) = H::start_with(true, "", false).await else {
        return;
    };
    h.report(json!([{"run_id": "r1", "provider": "openai", "model": "gpt-4o", "input_tokens": 10, "output_tokens": 1}])).await;
    h.flush(1).await;
    let r = h.json("/admin/orgs/acme/insights?days=1").await;
    assert_eq!(r["suggestions"]["available"], false);
    assert_eq!(r["suggestions"]["reason"], "not_configured");
    let resp = h
        .http
        .post(format!("{}/admin/orgs/acme/insights/suggest", h.admin))
        .bearer_auth(ADMIN_TOKEN)
        .json(&json!({"rule": "x"}))
        .send()
        .await
        .unwrap();
    // No license: the paid feature is refused before anything else.
    assert!(resp.status().is_client_error(), "{}", resp.status());
}
