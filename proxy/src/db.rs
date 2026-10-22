//! Postgres: the system of record for the managed config (every version is kept) and the admin
//! audit log. Optional: without a database URL both live in Redis, which is fine for trying
//! TokenLens out but keeps only the current config and the last audit entries.
//!
//! Redis stays the hot path either way (budgets, run state, live activity); nothing here runs
//! per request.

use std::time::Duration;

use anyhow::{Context, Result};
use chrono::{DateTime, SecondsFormat, Utc};
use serde_json::{Value, json};
use sqlx::Row;
use sqlx::postgres::{PgPool, PgPoolOptions};

use crate::audit::Entry;

static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");

#[derive(Clone)]
pub struct Db {
    pool: PgPool,
}

impl Db {
    /// Connect and bring the schema up to date. Migrations take a lock, so several instances
    /// starting at once are fine.
    pub async fn connect(url: &str, max_connections: u32) -> Result<Self> {
        let pool = PgPoolOptions::new()
            .max_connections(max_connections)
            .acquire_timeout(Duration::from_secs(5))
            .connect(url)
            .await
            .context("connecting to Postgres")?;
        MIGRATOR
            .run(&pool)
            .await
            .context("running database migrations")?;
        Ok(Self { pool })
    }

    pub async fn ping(&self) -> Result<()> {
        sqlx::query("SELECT 1").execute(&self.pool).await?;
        Ok(())
    }

    // --- Managed config ---

    pub async fn control_version(&self) -> Result<u64> {
        let v: Option<i64> = sqlx::query_scalar("SELECT max(version) FROM config_versions")
            .fetch_one(&self.pool)
            .await?;
        Ok(v.unwrap_or(0) as u64)
    }

    pub async fn control_load(&self) -> Result<Option<(u64, String)>> {
        let row: Option<(i64, Value)> = sqlx::query_as(
            "SELECT version, doc FROM config_versions ORDER BY version DESC LIMIT 1",
        )
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|(v, doc)| (v as u64, doc.to_string())))
    }

    /// Save `doc` as the next version if the current one is still `expected`, together with
    /// its audit entry. Returns the new version, or None if someone else saved first.
    pub async fn control_store(
        &self,
        expected: u64,
        doc: &str,
        actor: &str,
        audit: Option<&Entry>,
    ) -> Result<Option<u64>> {
        let next = expected as i64 + 1;
        let doc: Value = serde_json::from_str(doc)?;
        let mut tx = self.pool.begin().await?;
        // Versions are consecutive, so the primary key is the compare-and-set.
        let inserted = sqlx::query(
            "INSERT INTO config_versions (version, doc, actor) VALUES ($1, $2, $3) \
             ON CONFLICT (version) DO NOTHING",
        )
        .bind(next)
        .bind(&doc)
        .bind(actor)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        if inserted == 0 {
            return Ok(None);
        }
        if let Some(e) = audit {
            insert_audit(&mut tx, e, Some(next)).await?;
        }
        tx.commit().await?;
        Ok(Some(next as u64))
    }

    // --- Audit log ---

    pub async fn audit(&self, e: &Entry) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        insert_audit(&mut tx, e, None).await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn audit_list(
        &self,
        org: Option<&str>,
        before: Option<i64>,
        limit: usize,
    ) -> Result<Vec<Value>> {
        let rows = sqlx::query(
            "SELECT id, at, org_id, actor, action, target, detail, config_version, ip \
             FROM audit_log \
             WHERE ($1::text IS NULL OR org_id = $1) AND ($2::bigint IS NULL OR id < $2) \
             ORDER BY id DESC LIMIT $3",
        )
        .bind(org)
        .bind(before)
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await?;
        rows.iter()
            .map(|r| {
                let at: DateTime<Utc> = r.try_get("at")?;
                Ok(json!({
                    "id": r.try_get::<i64, _>("id")?.to_string(),
                    "at": at.to_rfc3339_opts(SecondsFormat::Millis, true),
                    "org": r.try_get::<Option<String>, _>("org_id")?,
                    "actor": r.try_get::<String, _>("actor")?,
                    "action": r.try_get::<String, _>("action")?,
                    "target": r.try_get::<String, _>("target")?,
                    "detail": r.try_get::<Value, _>("detail")?,
                    "config_version": r.try_get::<Option<i64>, _>("config_version")?,
                    "ip": r.try_get::<Option<String>, _>("ip")?,
                }))
            })
            .collect()
    }
}

async fn insert_audit(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    e: &Entry,
    config_version: Option<i64>,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO audit_log (org_id, actor, action, target, detail, config_version, ip) \
         VALUES ($1, $2, $3, $4, $5, $6, $7)",
    )
    .bind(&e.org)
    .bind(&e.actor)
    .bind(&e.action)
    .bind(&e.target)
    .bind(&e.detail)
    .bind(config_version)
    .bind(&e.ip)
    .execute(&mut **tx)
    .await?;
    Ok(())
}
