pub mod admin;
pub mod config;
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
    Ok(Arc::new(AppState {
        rt,
        ledger,
        http,
        prices,
    }))
}

pub fn spawn_sweeper(state: Arc<AppState>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(5));
        loop {
            tick.tick().await;
            let now = ledger::unix_now();
            for org in &state.rt.cfg.orgs {
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
