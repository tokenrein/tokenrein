//! Approval requests. A run whose budget is in Ask mode, or that loop detection paused, waits for
//! an approver instead of failing: it is paused, an alert goes out, and an approver either lets
//! it go on (giving a budget-limited run more budget) or stops it for good.
//!
//! Requests live in Redis next to the run's state and expire with it (7 days).

use std::collections::HashMap;

use anyhow::{Result, anyhow};
use chrono::{SecondsFormat, Utc};
use redis::AsyncCommands;
use serde_json::{Value, json};

use crate::alerts::{self, Alert, Severity};
use crate::audit::{self, Entry};
use crate::config::AlertEvent;
use crate::ledger::{Keys, RUN_TTL_SECS};
use crate::proxy::AppState;

/// Approval requests kept per org.
const KEEP: isize = 500;

/// Why a run is waiting.
pub enum Reason<'a> {
    /// Its budget (`scope`, e.g. `run:ticket-1`) is used up and in Ask mode.
    Budget {
        scope: &'a str,
        spent: i64,
        limit: i64,
    },
    /// Loop detection paused it.
    Loop { detail: &'a str },
}

pub struct Request<'a> {
    pub org: &'a str,
    pub run: &'a str,
    pub project: &'a str,
    pub agent: &'a str,
    pub reason: Reason<'a>,
}

fn now() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true)
}

fn usd(micros: i64) -> String {
    let v = micros as f64 / 1_000_000.0;
    // Small budgets (tests, per-call limits) need more than cents to make sense.
    if v.abs() < 1.0 {
        format!("${v:.4}")
    } else {
        format!("${v:.2}")
    }
}

/// Pause the run and ask for approval, unless a request is already waiting. Returns whether a
/// new request was made.
pub async fn request(st: &AppState, r: Request<'_>) -> Result<bool> {
    let keys = Keys::new(r.org);
    let key = keys.approval(r.run);
    let mut conn = st.ledger.conn();
    let (kind, scope, spent, limit, detail) = match &r.reason {
        Reason::Budget {
            scope,
            spent,
            limit,
        } => (
            "budget",
            scope.to_string(),
            *spent,
            *limit,
            format!("spent {} of its {} budget", usd(*spent), usd(*limit)),
        ),
        Reason::Loop { detail } => ("loop", format!("run:{}", r.run), 0, 0, detail.to_string()),
    };
    // HSETNX on `status` makes concurrent requests for one run create a single approval.
    let created: bool = conn.hset_nx(&key, "status", "pending").await?;
    if !created {
        let status: Option<String> = conn.hget(&key, "status").await?;
        if status.as_deref() == Some("pending") {
            return Ok(false);
        }
        // A decided request is replaced by the new one.
        let _: () = conn.hset(&key, "status", "pending").await?;
    }
    let _: () = redis::pipe()
        .hset_multiple(
            &key,
            &[
                ("run", r.run.to_string()),
                ("kind", kind.to_string()),
                ("scope", scope.clone()),
                ("spent", spent.to_string()),
                ("limit", limit.to_string()),
                ("detail", detail.clone()),
                ("project", r.project.to_string()),
                ("agent", r.agent.to_string()),
                ("requested_at", now()),
            ],
        )
        .hdel(&key, &["decided_by", "decided_at", "added"])
        .expire(&key, RUN_TTL_SECS as i64)
        .zadd(keys.approvals(), r.run, Utc::now().timestamp())
        .zremrangebyrank(keys.approvals(), 0, -(KEEP + 1))
        .query_async(&mut conn)
        .await?;
    st.ledger.set_run_state(r.org, r.run, "paused").await?;
    let entry = Entry {
        org: Some(r.org.to_string()),
        actor: "system".into(),
        action: "approval.requested".into(),
        target: r.run.to_string(),
        detail: json!({ "kind": kind, "scope": scope, "detail": detail }),
        ip: None,
    };
    audit::record(st, entry).await;
    let title = match kind {
        "budget" => format!("Run {} needs approval to spend more", r.run),
        _ => format!("Run {} was paused: it looks like a loop", r.run),
    };
    let alert = Alert {
        event: AlertEvent::ApprovalRequested,
        severity: Severity::Warning,
        title,
        text: format!(
            "{}/{}: {detail}. An approver can let it continue or stop it.",
            r.project, r.agent
        ),
        dedup: format!("approval:{}", r.run),
        quiet_secs: 600,
        tab: Some("approvals"),
        fields: vec![
            ("Run".into(), r.run.to_string()),
            ("Project".into(), r.project.to_string()),
            ("Agent".into(), r.agent.to_string()),
        ],
    };
    alerts::notify(st, r.org, alert).await;
    Ok(true)
}

/// Recent requests, newest first, and how many are waiting.
pub async fn list(st: &AppState, org: &str, limit: usize) -> Result<(Vec<Value>, usize)> {
    let keys = Keys::new(org);
    let mut conn = st.ledger.conn();
    let runs: Vec<String> = conn
        .zrevrange(keys.approvals(), 0, limit.max(1) as isize - 1)
        .await?;
    if runs.is_empty() {
        return Ok((Vec::new(), 0));
    }
    let mut pipe = redis::pipe();
    for run in &runs {
        pipe.hgetall(keys.approval(run));
    }
    let rows: Vec<HashMap<String, String>> = pipe.query_async(&mut conn).await?;
    let mut pending = 0;
    let out = rows
        .into_iter()
        .filter(|h| !h.is_empty())
        .map(|h| {
            let s = |k: &str| h.get(k).cloned();
            let n = |k: &str| h.get(k).and_then(|v| v.parse::<i64>().ok());
            if s("status").as_deref() == Some("pending") {
                pending += 1;
            }
            json!({
                "run_id": s("run"),
                "status": s("status"),
                "kind": s("kind"),
                "scope": s("scope"),
                "spent_micros": n("spent"),
                "limit_micros": n("limit"),
                "detail": s("detail"),
                "project": s("project"),
                "agent": s("agent"),
                "requested_at": s("requested_at"),
                "decided_by": s("decided_by"),
                "decided_at": s("decided_at"),
                "added_micros": n("added"),
            })
        })
        .collect();
    Ok((out, pending))
}

#[derive(Debug)]
pub enum DecideError {
    NotFound,
    NotPending(String),
    Invalid(&'static str),
    Unavailable(anyhow::Error),
}

impl From<anyhow::Error> for DecideError {
    fn from(e: anyhow::Error) -> Self {
        Self::Unavailable(e)
    }
}

impl From<redis::RedisError> for DecideError {
    fn from(e: redis::RedisError) -> Self {
        Self::Unavailable(anyhow!(e))
    }
}

/// Approve (optionally adding `add_usd` to the budget that ran out) or deny a waiting request.
/// Returns the run's new state.
pub async fn decide(
    st: &AppState,
    org: &str,
    run: &str,
    approve: bool,
    add_usd: Option<f64>,
    actor: &str,
) -> Result<&'static str, DecideError> {
    let keys = Keys::new(org);
    let key = keys.approval(run);
    let mut conn = st.ledger.conn();
    let h: HashMap<String, String> = conn.hgetall(&key).await?;
    match h.get("status").map(String::as_str) {
        None => return Err(DecideError::NotFound),
        Some("pending") => {}
        Some(other) => return Err(DecideError::NotPending(other.to_string())),
    }
    let add = match add_usd {
        Some(v) if !(v.is_finite() && (0.0..=1_000_000.0).contains(&v)) => {
            return Err(DecideError::Invalid(
                "add_usd must be between 0 and 1,000,000",
            ));
        }
        Some(v) => crate::config::usd_to_micros(v) as i64,
        None => 0,
    };
    let state = if approve {
        if add > 0 {
            // The budget that ran out: this run's, or an ancestor's named in the scope (spend of
            // sub-agent runs counts against their parents).
            let budget_run = h
                .get("scope")
                .and_then(|s| s.strip_prefix("run:"))
                .unwrap_or(run);
            let _: i64 = conn
                .hincr(keys.run_budget(budget_run), "limit", add)
                .await?;
        }
        st.ledger.clear_run_state(org, run).await?;
        "running"
    } else {
        st.ledger.set_run_state(org, run, "killed").await?;
        "killed"
    };
    let decided_at = now();
    let added = add.to_string();
    let status = if approve { "approved" } else { "denied" };
    let _: () = conn
        .hset_multiple(
            &key,
            &[
                ("status", status),
                ("decided_by", actor),
                ("decided_at", decided_at.as_str()),
                ("added", added.as_str()),
            ],
        )
        .await?;
    Ok(state)
}
