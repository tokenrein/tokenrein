//! Orgs, projects, virtual keys and budgets managed at runtime (admin API / dashboard).
//!
//! The managed copy lives in Redis as one JSON document with a version number. On first start
//! it is seeded from the config file's `[[orgs]]`; after that Redis is the source of truth and
//! the config file only supplies server, provider and pricing settings. Every proxy instance
//! polls the version and swaps in the new config when it changes; edits are compare-and-set,
//! so two admins (or two instances) cannot overwrite each other's changes.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use anyhow::{Context, Result};

use crate::config::OrgConfig;
use crate::proxy::AppState;

const SYNC_EVERY: Duration = Duration::from_secs(2);

/// Load the managed config from Redis, seeding it from the config file if it is not there yet.
pub async fn init(state: &AppState) -> Result<()> {
    if let Some((version, doc)) = state.ledger.control_load().await? {
        return apply(state, version, &doc).context("managed config in Redis is invalid");
    }
    let doc = serde_json::to_string(&state.rt().cfg.orgs)?;
    match state.ledger.control_store(0, &doc).await? {
        Some(version) => {
            tracing::info!(
                version,
                "seeded managed orgs/keys/budgets from the config file"
            );
            state.control_version.store(version, Ordering::SeqCst);
            Ok(())
        }
        // Another instance seeded it first.
        None => sync(state).await,
    }
}

/// Pick up changes written by any instance.
pub async fn sync(state: &AppState) -> Result<()> {
    let current = state.control_version.load(Ordering::SeqCst);
    if state.ledger.control_version().await? == current {
        return Ok(());
    }
    if let Some((version, doc)) = state.ledger.control_load().await? {
        apply(state, version, &doc)?;
        tracing::info!(version, "reloaded managed orgs/keys/budgets");
    }
    Ok(())
}

fn apply(state: &AppState, version: u64, doc: &str) -> Result<()> {
    let orgs: Vec<OrgConfig> = serde_json::from_str(doc)?;
    let rt = state.rt().with_orgs(orgs)?;
    state.replace_rt(rt, version);
    Ok(())
}

pub fn spawn_sync(state: Arc<AppState>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(SYNC_EVERY);
        loop {
            tick.tick().await;
            if let Err(e) = sync(&state).await {
                tracing::error!(error = %e, "managed config sync failed; keeping the current config");
            }
        }
    })
}

#[derive(Debug)]
pub enum EditError {
    /// The edit is not valid (bad id, unknown project, invalid budget, ...).
    Invalid(String),
    NotFound(String),
    /// The managed config has not been initialised (`init` was not called).
    Disabled,
    /// Someone else changed the config at the same time; retry.
    Conflict,
    Unavailable(anyhow::Error),
}

/// Apply `edit` to the current orgs, validate the result, store it and switch to it.
pub async fn edit<T>(
    state: &AppState,
    edit: impl FnOnce(&mut Vec<OrgConfig>) -> Result<T, EditError>,
) -> Result<T, EditError> {
    let _guard = state.control_edit.lock().await;
    sync(state).await.map_err(EditError::Unavailable)?;
    let version = state.control_version.load(Ordering::SeqCst);
    if version == 0 {
        return Err(EditError::Disabled);
    }
    let rt = state.rt();
    let mut orgs = rt.cfg.orgs.clone();
    let out = edit(&mut orgs)?;
    let next = rt
        .with_orgs(orgs)
        .map_err(|e| EditError::Invalid(format!("{e:#}")))?;
    let doc =
        serde_json::to_string(&next.cfg.orgs).map_err(|e| EditError::Unavailable(e.into()))?;
    match state.ledger.control_store(version, &doc).await {
        Ok(Some(v)) => {
            state.replace_rt(next, v);
            Ok(out)
        }
        Ok(None) => Err(EditError::Conflict),
        Err(e) => Err(EditError::Unavailable(e)),
    }
}
