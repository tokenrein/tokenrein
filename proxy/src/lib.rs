pub mod admin;
pub mod config;
pub mod control;
pub mod ledger;
pub mod pricing;
pub mod providers;
pub mod proxy;

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};

pub use proxy::AppState;

pub async fn build_state(rt: config::Resolved) -> Result<Arc<AppState>> {
    let ledger = ledger::Ledger::connect(&rt.cfg.redis.url)
        .await
        .context("connecting to Redis")?;
    let prices = pricing::Catalog::with_overrides(&rt.cfg.pricing)?;
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(rt.cfg.server.upstream_timeout_secs))
        .connect_timeout(Duration::from_secs(10))
        .pool_idle_timeout(Duration::from_secs(90))
        .build()
        .context("building HTTP client")?;
    let (activity, events) = tokio::sync::mpsc::channel(ACTIVITY_QUEUE);
    let state = Arc::new(AppState::new(rt, ledger, http, prices, activity));
    spawn_activity_writer(state.clone(), events);
    Ok(state)
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
                match state.ledger.sweep(&org.id, now, 500).await {
                    Ok(0) => {}
                    Ok(n) => {
                        tracing::warn!(org = %org.id, released = n, "released expired reservations")
                    }
                    Err(e) => {
                        tracing::error!(org = %org.id, error = %e, "reservation sweep failed")
                    }
                }
            }
        }
    })
}
