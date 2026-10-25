pub mod admin;
pub mod audit;
pub mod auth;
pub mod config;
pub mod connectors;
pub mod control;
pub mod db;
pub mod ingest;
pub mod ledger;
pub mod metrics;
pub mod packs;
pub mod pricing;
pub mod providers;
pub mod proxy;
pub mod secrets;

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};

pub use proxy::AppState;

pub async fn build_state(rt: config::Resolved) -> Result<Arc<AppState>> {
    let timeout = Duration::from_millis(rt.cfg.redis.timeout_ms.max(1));
    let ledger = ledger::Ledger::connect_with_timeout(&rt.cfg.redis.url, timeout)
        .await
        .context("connecting to Redis")?;
    let db = match &rt.database_url {
        Some(url) => Some(db::Db::connect(url, rt.cfg.database.max_connections).await?),
        None => None,
    };
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(rt.cfg.server.upstream_timeout_secs))
        .connect_timeout(Duration::from_secs(10))
        .pool_idle_timeout(Duration::from_secs(90))
        .build()
        .context("building HTTP client")?;
    let (activity, events) = tokio::sync::mpsc::channel(ACTIVITY_QUEUE);
    let state = Arc::new(AppState::new(rt, ledger, db, http, activity));
    spawn_activity_writer(state.clone(), events);
    Ok(state)
}

/// `tokenrein rotate-secrets-key`: connect to the stores in `rt` and re-encrypt saved
/// provider keys under the current secrets key (see [`control::rotate_secrets_key`]).
pub async fn rotate_secrets_key(rt: config::Resolved) -> Result<Vec<String>> {
    let state = build_state(rt).await?;
    control::init(&state)
        .await
        .context("loading the managed config")?;
    let mut resealed = control::rotate_secrets_key(&state, "system:rotate-secrets-key").await?;
    if auth::oidc::SsoConfig::rotate_secret(&state).await? {
        resealed.push("single sign-on client secret".into());
    }
    Ok(resealed)
}

/// `tokenrein probe URL...`: a healthcheck for images without curl. Fails unless every
/// URL answers 2xx within 3 seconds.
pub async fn probe(urls: &[String]) -> Result<()> {
    anyhow::ensure!(!urls.is_empty(), "usage: tokenrein probe URL...");
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(3))
        .build()?;
    for url in urls {
        let status = http
            .get(url)
            .send()
            .await
            .with_context(|| format!("GET {url}"))?
            .status();
        anyhow::ensure!(status.is_success(), "GET {url}: {status}");
    }
    Ok(())
}

/// Request events waiting to be written for the dashboard; more are dropped.
const ACTIVITY_QUEUE: usize = 10_000;
const ACTIVITY_BATCH: usize = 200;

fn spawn_activity_writer(
    state: Arc<AppState>,
    mut events: tokio::sync::mpsc::Receiver<serde_json::Value>,
) -> tokio::task::JoinHandle<()> {
    // Holds a Weak reference so the state (and this task) can be dropped.
    let weak = Arc::downgrade(&state);
    drop(state);
    tokio::spawn(async move {
        let mut batch = Vec::with_capacity(ACTIVITY_BATCH);
        while events.recv_many(&mut batch, ACTIVITY_BATCH).await > 0 {
            let Some(state) = weak.upgrade() else { return };
            if let Err(e) = state.ledger.record_activity(&batch).await {
                tracing::warn!(error = %e, dropped = batch.len(), "could not record dashboard activity");
                metrics::activity_write_failed(batch.len());
            }
            batch.clear();
        }
    })
}

pub fn spawn_sweeper(state: Arc<AppState>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(5));
        loop {
            tick.tick().await;
            let now = ledger::unix_now();
            let rt = state.rt();
            for org in &rt.cfg.orgs {
                if now % 3600 < 5
                    && let Err(e) = state.ledger.trim_activity(&org.id).await
                {
                    tracing::warn!(org = %org.id, error = %e, "activity trim failed");
                }
                if now % 3600 < 5 && org.cursor_import.is_some() {
                    connectors::sync_due(&state, &org.id).await;
                }
                match state.ledger.sweep(&org.id, now, 500).await {
                    Ok(0) => {}
                    Ok(n) => {
                        tracing::warn!(org = %org.id, released = n, "released expired reservations")
                    }
                    Err(e) => {
                        tracing::error!(org = %org.id, error = %e, "reservation sweep failed");
                        metrics::ledger_error();
                    }
                }
            }
        }
    })
}
