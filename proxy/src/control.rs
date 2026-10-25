//! Orgs, projects, virtual keys, budgets and model prices managed at runtime (admin API /
//! dashboard).
//!
//! The managed copy is one JSON document with a version number, kept in Postgres (every version,
//! with who saved it) or, without a database, in Redis (the current version only). On first
//! start it is seeded from the config file's `[[orgs]]` and `[[pricing]]` (or, when a database is
//! added later, from the copy already in Redis); after that the store is the source of truth and
//! the config file only supplies server and provider settings. Every proxy instance polls the
//! version and swaps in the new config when it changes; edits are compare-and-set, so two admins
//! (or two instances) cannot overwrite each other's changes. Each edit is written to the audit
//! log with the fields it changed.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use anyhow::{Context, Result};

use serde::{Deserialize, Serialize};

use crate::audit::{self, Change, Entry};
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

async fn load(state: &AppState) -> Result<Option<(u64, String)>> {
    match &state.db {
        Some(db) => db.control_load().await,
        None => state.ledger.control_load().await,
    }
}

async fn stored_version(state: &AppState) -> Result<u64> {
    match &state.db {
        Some(db) => db.control_version().await,
        None => state.ledger.control_version().await,
    }
}

async fn store(
    state: &AppState,
    expected: u64,
    doc: &str,
    actor: &str,
    entry: Option<&Entry>,
) -> Result<Option<u64>> {
    let version = match &state.db {
        Some(db) => return db.control_store(expected, doc, actor, entry).await,
        None => state.ledger.control_store(expected, doc).await?,
    };
    if let (Some(v), Some(e)) = (version, entry)
        && let Err(err) = state.ledger.audit_append(e, Some(v)).await
    {
        tracing::error!(error = %err, action = %e.action, "could not write the audit log");
        crate::metrics::audit_write_failed();
    }
    Ok(version)
}

/// Load the managed config, seeding it if it is not there yet.
pub async fn init(state: &AppState) -> Result<()> {
    if let Some((version, doc)) = load(state).await? {
        return apply(state, version, &doc).context("stored managed config is invalid");
    }
    // A database added to an existing install takes over the config kept in Redis.
    let (doc, from) = match (&state.db, state.ledger.control_load().await?) {
        (Some(_), Some((_, doc))) => {
            parse(&doc).context("managed config in Redis is invalid")?;
            (doc, "Redis")
        }
        _ => {
            let rt = state.rt();
            let doc = serde_json::to_string(&Managed {
                orgs: rt.cfg.orgs.clone(),
                pricing: rt.cfg.pricing.clone(),
            })?;
            (doc, "the config file")
        }
    };
    match store(state, 0, &doc, "system", None).await? {
        Some(version) => {
            tracing::info!(version, from, "seeded managed orgs/keys/budgets");
            apply(state, version, &doc)
        }
        // Another instance seeded it first.
        None => sync(state).await,
    }
}

/// Pick up changes written by any instance.
pub async fn sync(state: &AppState) -> Result<()> {
    let current = state.control_version.load(Ordering::SeqCst);
    if stored_version(state).await? == current {
        return Ok(());
    }
    if let Some((version, doc)) = load(state).await? {
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

/// Apply `edit` to the current orgs, validate the result, store it and switch to it. `change`
/// says who is making the edit, for the audit log.
pub async fn edit<T>(
    state: &AppState,
    change: Change,
    edit: impl FnOnce(&mut Vec<OrgConfig>) -> Result<T, EditError>,
) -> Result<T, EditError> {
    edit_managed(state, change, |m| edit(&mut m.orgs)).await
}

/// Like [`edit`] for the model price overrides.
pub async fn edit_pricing<T>(
    state: &AppState,
    change: Change,
    edit: impl FnOnce(&mut Vec<PriceOverride>) -> Result<T, EditError>,
) -> Result<T, EditError> {
    edit_managed(state, change, |m| edit(&mut m.pricing)).await
}

async fn edit_managed<T>(
    state: &AppState,
    change: Change,
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
    let before = serde_json::to_value(&m).map_err(|e| EditError::Unavailable(e.into()))?;
    let out = edit(&mut m)?;
    let next = rt
        .with_managed(m.orgs, m.pricing)
        .map_err(|e| EditError::Invalid(format!("{e:#}")))?;
    let after = serde_json::to_value(Managed {
        orgs: next.cfg.orgs.clone(),
        pricing: next.cfg.pricing.clone(),
    })
    .map_err(|e| EditError::Unavailable(e.into()))?;
    let changes = audit::diff(&before, &after);
    if changes.is_empty() {
        return Ok(out);
    }
    let entry = change.entry(serde_json::json!({ "changes": changes }));
    match store(
        state,
        version,
        &after.to_string(),
        &change.actor,
        Some(&entry),
    )
    .await
    {
        Ok(Some(v)) => {
            audit::logged(&entry, v);
            state.replace_rt(next, v);
            Ok(out)
        }
        Ok(None) => Err(EditError::Conflict),
        Err(e) => Err(EditError::Unavailable(e)),
    }
}

/// Re-encrypts every provider key saved from the dashboard under the current secrets key
/// (`TOKENREIN_SECRETS_KEY`). Keys sealed under the previous one are opened with
/// `TOKENREIN_SECRETS_KEY_PREVIOUS`; keys already under the current key are left alone. Nothing is saved
/// unless every key can be opened. Returns the `org/provider` pairs that were re-encrypted.
pub async fn rotate_secrets_key(state: &AppState, actor: &str) -> Result<Vec<String>> {
    let _guard = state.control_edit.lock().await;
    sync(state).await?;
    let version = state.control_version.load(Ordering::SeqCst);
    anyhow::ensure!(version != 0, "managed config is not initialised");
    let rt = state.rt();
    let var = &rt.cfg.server.secrets_key_env;
    let current = rt
        .secrets()
        .with_context(|| format!("{var} (the new secrets key) is not set"))?;
    let previous = rt.previous_secrets();
    let mut m = Managed {
        orgs: rt.cfg.orgs.clone(),
        pricing: rt.cfg.pricing.clone(),
    };
    let (mut resealed, mut unreadable) = (Vec::new(), Vec::new());
    for org in &mut m.orgs {
        for (provider, saved) in &mut org.provider_secrets {
            let ctx = crate::secrets::context(&org.id, provider);
            if current.open(&ctx, &saved.sealed).is_ok() {
                continue;
            }
            match previous.and_then(|p| p.open(&ctx, &saved.sealed).ok()) {
                Some(plain) => {
                    saved.sealed = current.seal(&ctx, &plain)?;
                    resealed.push(format!("{}/{provider}", org.id));
                }
                None => unreadable.push(format!("{}/{provider}", org.id)),
            }
        }
    }
    if !unreadable.is_empty() {
        anyhow::bail!(
            "neither {var} nor {var}_PREVIOUS can decrypt the saved keys for {}; nothing was changed",
            unreadable.join(", ")
        );
    }
    if resealed.is_empty() {
        return Ok(resealed);
    }
    resealed.sort();
    let next = rt.with_managed(m.orgs, m.pricing)?;
    let doc = serde_json::to_string(&Managed {
        orgs: next.cfg.orgs.clone(),
        pricing: next.cfg.pricing.clone(),
    })?;
    // The audit diff hides sealed values, so record which keys were re-encrypted instead.
    let entry = Change {
        actor: actor.to_string(),
        ip: None,
        org: None,
        action: "secrets_key.rotate",
        target: "provider_secrets".into(),
    }
    .entry(serde_json::json!({ "resealed": resealed }));
    match store(state, version, &doc, actor, Some(&entry)).await? {
        Some(v) => {
            audit::logged(&entry, v);
            state.replace_rt(next, v);
            Ok(resealed)
        }
        None => anyhow::bail!("the config changed while rotating; run the command again"),
    }
}
