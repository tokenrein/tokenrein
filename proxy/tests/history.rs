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
            [[orgs]]
            id = "acme"
            [[orgs.projects]]
            id = "bot"
            virtual_key_hashes = ["{hash}"]
            "#,
            redis = redis.url,
            hash = sha256_hex(KEY),
        );
        let rt = Resolved::from_toml(&toml, |name| match name {
            "ADMIN" => Some(ADMIN_TOKEN.into()),
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
