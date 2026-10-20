//! Orgs, projects, virtual keys, budgets and model prices managed at runtime (admin API /
//! dashboard).
//!
//! The managed copy lives in Redis as one JSON document with a version number. On first start
//! it is seeded from the config file's `[[orgs]]` and `[[pricing]]`; after that Redis is the
//! source of truth and the config file only supplies server and provider settings. Every proxy instance
//! polls the version and swaps in the new config when it changes; edits are compare-and-set,
//! so two admins (or two instances) cannot overwrite each other's changes.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use anyhow::{Context, Result};

use serde::{Deserialize, Serialize};

use crate::config::{OrgConfig, PriceOverride};
use crate::proxy::AppState;

/// The managed document. Older versions stored a bare array of orgs.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Managed {
    pub orgs: Vec<OrgConfig>,
    #[serde(default)]
    pub pricing: Vec<PriceOverride>,
}

fn parse(doc: &str) -> Result<Managed> {
    if doc.trim_start().starts_with('[') {
        return Ok(Managed {
            orgs: serde_json::from_str(doc)?,
            pricing: Vec::new(),
        });
    }
    Ok(serde_json::from_str(doc)?)
}

const SYNC_EVERY: Duration = Duration::from_secs(2);

/// Load the managed config from Redis, seeding it from the config file if it is not there yet.
pub async fn init(state: &AppState) -> Result<()> {
    if let Some((version, doc)) = state.ledger.control_load().await? {
        return apply(state, version, &doc).context("managed config in Redis is invalid");
    }
    let rt = state.rt();
    let doc = serde_json::to_string(&Managed {
        orgs: rt.cfg.orgs.clone(),
        pricing: rt.cfg.pricing.clone(),
    })?;
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
    let m = parse(doc)?;
    let rt = state.rt().with_managed(m.orgs, m.pricing)?;
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
    edit_managed(state, |m| edit(&mut m.orgs)).await
}

/// Like [`edit`] for the model price overrides.
pub async fn edit_pricing<T>(
    state: &AppState,
    edit: impl FnOnce(&mut Vec<PriceOverride>) -> Result<T, EditError>,
) -> Result<T, EditError> {
    edit_managed(state, |m| edit(&mut m.pricing)).await
}

async fn edit_managed<T>(
    state: &AppState,
    edit: impl FnOnce(&mut Managed) -> Result<T, EditError>,
) -> Result<T, EditError> {
    let _guard = state.control_edit.lock().await;
    sync(state).await.map_err(EditError::Unavailable)?;
    let version = state.control_version.load(Ordering::SeqCst);
    if version == 0 {
        return Err(EditError::Disabled);
    }
    let rt = state.rt();
    let mut m = Managed {
        orgs: rt.cfg.orgs.clone(),
        pricing: rt.cfg.pricing.clone(),
    };
    let out = edit(&mut m)?;
    let next = rt
        .with_managed(m.orgs, m.pricing)
        .map_err(|e| EditError::Invalid(format!("{e:#}")))?;
    let doc = serde_json::to_string(&Managed {
        orgs: next.cfg.orgs.clone(),
        pricing: next.cfg.pricing.clone(),
    })
    .map_err(|e| EditError::Unavailable(e.into()))?;
    match state.ledger.control_store(version, &doc).await {
        Ok(Some(v)) => {
            state.replace_rt(next, v);
            Ok(out)
        }
        Ok(None) => Err(EditError::Conflict),
        Err(e) => Err(EditError::Unavailable(e)),
    }
}
