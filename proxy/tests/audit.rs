//! Config history and the admin audit log, with Postgres and without (Redis only).

mod common;

use std::sync::Arc;

use reqwest::{Method, StatusCode};
use serde_json::{Value, json};
use tokenlens_proxy::config::Resolved;
use tokenlens_proxy::{AppState, admin, build_state, control};

const ADMIN_TOKEN: &str = "admin-token-0123456789";
const SECRETS_KEY: &str = "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f";

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

async fn instance(redis: &str, db: Option<&str>) -> Arc<AppState> {
    let db = db.map(str::to_string);
    let rt = Resolved::from_toml(&config(redis), move |name| match name {
        "ADMIN" => Some(ADMIN_TOKEN.into()),
        "SECRETS" => Some(SECRETS_KEY.into()),
        "DB" => db.clone(),
        _ => None,
    })
    .unwrap();
    let state = build_state(rt).await.unwrap();
    control::init(&state).await.unwrap();
    state
}

struct Admin {
    url: String,
    http: reqwest::Client,
}

impl Admin {
    async fn serve(state: Arc<AppState>) -> Self {
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
        Self {
            url: format!("http://{addr}"),
            http: reqwest::Client::new(),
        }
    }

    async fn call(&self, method: Method, path: &str, body: Option<Value>) -> (StatusCode, Value) {
        let mut req = self
            .http
            .request(method, format!("{}{path}", self.url))
            .bearer_auth(ADMIN_TOKEN);
        if let Some(b) = body {
            req = req.json(&b);
        }
        let resp = req.send().await.unwrap();
        let status = resp.status();
        (status, resp.json().await.unwrap_or(Value::Null))
    }

    async fn audit(&self, query: &str) -> Value {
        let (status, body) = self
            .call(Method::GET, &format!("/admin/audit?{query}"), None)
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        body
    }
}

fn actions(page: &Value) -> Vec<&str> {
    page["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["action"].as_str().unwrap())
        .collect()
}

/// The changes an audit entry lists, as `path: before -> after`.
fn changes(entry: &Value) -> Vec<String> {
    entry["detail"]["changes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| {
            format!(
                "{}: {} -> {}",
                c["path"].as_str().unwrap(),
                c["before"],
                c["after"]
            )
        })
        .collect()
}

/// The same admin actions, audited the same way, whatever stores them.
async fn audits_admin_changes(admin: &Admin) {
    let budgets = json!({"budgets": {"run": {"limit_usd": 2.5, "mode": "enforce"}}});
    let p = "/admin/orgs/acme/projects/bot";
    assert_eq!(
        admin.call(Method::PUT, p, Some(budgets.clone())).await.0,
        StatusCode::OK
    );
    // Saving the same thing again changes nothing and is not logged.
    assert_eq!(
        admin.call(Method::PUT, p, Some(budgets)).await.0,
        StatusCode::OK
    );
    let (status, key) = admin
        .call(
            Method::POST,
            &format!("{p}/keys"),
            Some(json!({"name": "ci"})),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let secret = "sk-live-abcdefghijklmnop1234";
    let (status, _) = admin
        .call(
            Method::PUT,
            "/admin/orgs/acme/provider-keys/openai",
            Some(json!({"api_key": secret})),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let run = "/admin/orgs/acme/runs/job-1";
    assert_eq!(
        admin
            .call(Method::POST, &format!("{run}/pause"), None)
            .await
            .0,
        StatusCode::OK
    );
    assert_eq!(
        admin
            .call(Method::POST, &format!("{run}/kill"), None)
            .await
            .0,
        StatusCode::OK
    );

    let page = admin.audit("").await;
    assert_eq!(
        actions(&page),
        [
            "run.kill",
            "run.pause",
            "provider_key.set",
            "key.create",
            "project.update"
        ]
    );
    let entries = page["entries"].as_array().unwrap();
    for e in entries {
        assert_eq!(e["actor"], "admin-token");
        assert_eq!(e["org"], "acme");
        assert_eq!(e["ip"], "127.0.0.1");
        assert!(e["at"].as_str().unwrap().ends_with('Z'));
    }
    assert_eq!(entries[1]["target"], "job-1");
    assert_eq!(
        changes(&entries[4]),
        [r#"orgs[acme].projects[bot].budgets.run: null -> {"limit_usd":2.5,"mode":"enforce"}"#]
    );
    let key_id = key["id"].as_str().unwrap();
    assert_eq!(entries[3]["target"], format!("bot/{key_id}"));
    let key_changes = changes(&entries[3]);
    assert!(
        key_changes[0].starts_with(&format!(
            "orgs[acme].projects[bot].keys[{key_id}]: null -> "
        )),
        "{key_changes:?}"
    );
    // Neither the provider key nor its ciphertext is ever in the log.
    let text = page.to_string();
    assert!(
        !text.contains(secret) && !text.contains("abcdefgh"),
        "{text}"
    );
    assert!(text.contains("[redacted]"));
    // Config entries point at the version they created; run actions don't change config.
    assert!(entries[4]["config_version"].as_u64().unwrap() >= 1);
    assert_eq!(entries[0]["config_version"], Value::Null);

    // Pages: `next` continues where the last page stopped.
    let first = admin.audit("limit=2").await;
    assert_eq!(actions(&first), ["run.kill", "run.pause"]);
    let next = first["next"].as_str().unwrap();
    let second = admin.audit(&format!("limit=2&before={next}")).await;
    assert_eq!(actions(&second), ["provider_key.set", "key.create"]);
    let last = admin
        .audit(&format!(
            "limit=2&before={}",
            second["next"].as_str().unwrap()
        ))
        .await;
    assert_eq!(actions(&last), ["project.update"]);
    assert_eq!(last["next"], Value::Null);

    // Instance-wide changes have no org, so an org filter leaves them out.
    let prices = json!({"overrides": [{"provider": "openai", "model": "my-model",
        "input_per_mtok": 1.0, "output_per_mtok": 2.0}]});
    assert_eq!(
        admin
            .call(Method::PUT, "/admin/pricing", Some(prices))
            .await
            .0,
        StatusCode::OK
    );
    assert_eq!(actions(&admin.audit("limit=1").await), ["pricing.update"]);
    assert_eq!(
        admin.audit("limit=1").await["entries"][0]["org"],
        Value::Null
    );
    assert_eq!(
        actions(&admin.audit("limit=1&org=acme").await),
        ["run.kill"]
    );

    // CSV download: a header, one row per entry, formulas defused.
    let resp = admin
        .http
        .get(format!("{}/admin/audit?format=csv", admin.url))
        .bearer_auth(ADMIN_TOKEN)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.headers()["content-type"], "text/csv; charset=utf-8");
    let csv = resp.text().await.unwrap();
    let lines: Vec<&str> = csv.lines().collect();
    assert_eq!(
        lines[0],
        "id,time,org,actor,action,target,ip,config_version,changes"
    );
    assert_eq!(lines.len(), 7, "{csv}");
    assert!(lines[1].contains(",pricing.update,"), "{csv}");

    // The log is for admins only.
    let resp = admin
        .http
        .get(format!("{}/admin/audit", admin.url))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn postgres_keeps_config_history_and_audit_log() {
    let Some(redis) = common::RedisServer::start() else {
        return;
    };
    let Some(pg) = common::PostgresServer::start() else {
        return;
    };
    let state = instance(&redis.url, Some(&pg.url)).await;
    let admin = Admin::serve(state.clone()).await;
    audits_admin_changes(&admin).await;
    assert_eq!(admin.audit("").await["durable"], true);

    // Every version is kept: seed + budget + key + provider key + prices.
    let pool = sqlx::PgPool::connect(&pg.url).await.unwrap();
    let versions: Vec<(i64, String)> =
        sqlx::query_as("SELECT version, actor FROM config_versions ORDER BY version")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(
        versions,
        [
            (1, "system".into()),
            (2, "admin-token".into()),
            (3, "admin-token".into()),
            (4, "admin-token".into()),
            (5, "admin-token".into())
        ]
    );
    // Redis no longer holds the managed config.
    let mut conn = redis::Client::open(redis.url.as_str())
        .unwrap()
        .get_multiplexed_async_connection()
        .await
        .unwrap();
    let doc: Option<String> = redis::cmd("GET")
        .arg("tl:control:orgs")
        .query_async(&mut conn)
        .await
        .unwrap();
    assert_eq!(doc, None);

    // A second instance starts from the stored config, not the file, and sees later changes.
    let other = instance(&redis.url, Some(&pg.url)).await;
    assert_eq!(other.rt().cfg.orgs[0].projects[0].keys.len(), 1);
    let body = json!({"id": "second"});
    let (status, _) = admin
        .call(Method::POST, "/admin/orgs/acme/projects", Some(body))
        .await;
    assert_eq!(status, StatusCode::CREATED);
    control::sync(&other).await.unwrap();
    assert_eq!(other.rt().cfg.orgs[0].projects.len(), 2);
}

#[tokio::test]
async fn redis_only_mode_keeps_the_audit_log_in_redis() {
    let Some(redis) = common::RedisServer::start() else {
        return;
    };
    let state = instance(&redis.url, None).await;
    let admin = Admin::serve(state).await;
    audits_admin_changes(&admin).await;
    assert_eq!(admin.audit("").await["durable"], false);
}

#[tokio::test]
async fn a_new_database_takes_over_the_config_in_redis() {
    let Some(redis) = common::RedisServer::start() else {
        return;
    };
    let Some(pg) = common::PostgresServer::start() else {
        return;
    };
    let before = instance(&redis.url, None).await;
    let admin = Admin::serve(before).await;
    let body = json!({"id": "made-in-redis"});
    let (status, _) = admin
        .call(Method::POST, "/admin/orgs/acme/projects", Some(body))
        .await;
    assert_eq!(status, StatusCode::CREATED);

    let after = instance(&redis.url, Some(&pg.url)).await;
    let projects: Vec<String> = after.rt().cfg.orgs[0]
        .projects
        .iter()
        .map(|p| p.id.clone())
        .collect();
    assert_eq!(projects, ["bot", "made-in-redis"]);
    assert_eq!(
        after
            .control_version
            .load(std::sync::atomic::Ordering::SeqCst),
        1
    );
}

#[tokio::test]
async fn concurrent_edits_from_two_instances_do_not_overwrite_each_other() {
    let Some(redis) = common::RedisServer::start() else {
        return;
    };
    let Some(pg) = common::PostgresServer::start() else {
        return;
    };
    let a = Admin::serve(instance(&redis.url, Some(&pg.url)).await).await;
    let b = Admin::serve(instance(&redis.url, Some(&pg.url)).await).await;
    let mut tasks = Vec::new();
    for i in 0..10 {
        let admin = if i % 2 == 0 { &a } else { &b };
        let body = json!({"id": format!("p{i}")});
        tasks.push(admin.call(Method::POST, "/admin/orgs/acme/projects", Some(body)));
    }
    let results = futures_util::future::join_all(tasks).await;
    let created = results
        .iter()
        .filter(|(s, _)| *s == StatusCode::CREATED)
        .count();
    // Edits that lost a race get 409 and change nothing; the rest are all kept.
    assert!(
        results
            .iter()
            .all(|(s, _)| *s == StatusCode::CREATED || *s == StatusCode::CONFLICT)
    );
    let state = instance(&redis.url, Some(&pg.url)).await;
    assert_eq!(state.rt().cfg.orgs[0].projects.len(), 1 + created);
    let page = a.audit("limit=100").await;
    assert_eq!(
        actions(&page)
            .iter()
            .filter(|a| **a == "project.create")
            .count(),
        created
    );
}
