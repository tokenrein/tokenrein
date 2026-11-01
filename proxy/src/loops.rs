//! Runaway-loop detection. Budgets stop a loop once the money is gone; this stops it early, from
//! the shape of the run:
//!
//! - **repeats**: the same request (same model and last message) or the same tool call (same
//!   tool and input) `max_repeats` times within `repeat_window_secs`;
//! - **velocity**: more than `max_usd_per_minute` spent within one minute;
//! - **calls**: more than `max_calls_per_run` model calls in one run.
//!
//! When one trips, the run gets a `loop_detected` alert and, by policy, is paused for an approver
//! (`pause`, the default), stopped (`kill`) or left running (`warn`). Each run trips once; an
//! approver letting it continue resets that. Only hashes of requests are kept, never content.

use anyhow::Result;
use redis::AsyncCommands;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::alerts::{self, Alert, Severity};
use crate::approvals::{self, Reason, Request};
use crate::audit::{self, Entry};
use crate::config::{AlertEvent, Budgets, LoopAction, LoopPolicy, Resolved};
use crate::ledger::{Keys, RUN_TTL_SECS};
use crate::proxy::AppState;

/// One thing a run did.
pub struct Seen<'a> {
    pub org: &'a str,
    pub project: &'a str,
    pub agent: &'a str,
    pub run: &'a str,
    /// Hash of the request or tool call, to spot repeats.
    pub fingerprint: Option<&'a str>,
    /// A model call completed (or is about to be made, in the proxy).
    pub call: bool,
    /// What it cost, in micro-USD.
    pub cost: u64,
}

/// The project's loop policy (inherited from the org).
pub fn policy_for(rt: &Resolved, org: &str, project: &str) -> Option<LoopPolicy> {
    let o = rt.cfg.orgs.iter().find(|o| o.id == org)?;
    let p = o.projects.iter().find(|p| p.id == project)?;
    Budgets::effective(&o.budgets, &p.budgets).loops
}

/// A short hash of the parts (never stored or sent in the clear).
pub fn fingerprint(parts: &[&str]) -> String {
    let mut h = Sha256::new();
    for p in parts {
        h.update(p.as_bytes());
        h.update([0u8]);
    }
    hex::encode(&h.finalize()[..16])
}

/// The fingerprint of an OpenAI or Anthropic request body: its model and last message.
pub fn request_fingerprint(model: &str, body: &Value) -> Option<String> {
    let last = body
        .get("messages")
        .and_then(Value::as_array)
        .and_then(|m| m.last())
        .or_else(|| match body.get("input") {
            Some(Value::Array(items)) => items.last(),
            Some(v @ Value::String(_)) => Some(v),
            _ => None,
        })?;
    Some(fingerprint(&[model, &last.to_string()]))
}

/// Whether a client-sent fingerprint looks valid (16-128 hex characters).
pub fn valid_fingerprint(f: &str) -> bool {
    (16..=128).contains(&f.len()) && f.bytes().all(|b| b.is_ascii_hexdigit())
}

enum Trip {
    Repeats(u64),
    Velocity(u64),
    Calls(u64),
}

impl Trip {
    fn describe(&self, p: &LoopPolicy) -> String {
        match self {
            Trip::Repeats(n) => format!(
                "the same request {n} times within {} minutes",
                p.repeat_window_secs.div_ceil(60)
            ),
            Trip::Velocity(m) => format!(
                "${:.2} spent within one minute (limit ${:.2})",
                *m as f64 / 1e6,
                p.max_usd_per_minute.unwrap_or_default()
            ),
            Trip::Calls(n) => format!("{n} model calls in one run"),
        }
    }
}

/// Count what the run did. Returns the action when this observation tripped the run's loop
/// guard and the run must stop now (`pause` or `kill`); `warn` only alerts.
pub async fn observe(
    st: &AppState,
    policy: Option<LoopPolicy>,
    seen: Seen<'_>,
) -> Result<Option<LoopAction>> {
    let Some(p) = policy else { return Ok(None) };
    let keys = Keys::new(seen.org);
    let mut conn = st.ledger.conn();
    let mut trip = None;
    if let Some(fp) = seen.fingerprint {
        let key = keys.loop_key(seen.run, &format!("fp:{fp}"));
        let n: u64 = conn.incr(&key, 1).await?;
        if n == 1 {
            let counted = keys.loop_key(seen.run, "fps");
            let _: () = redis::pipe()
                .expire(&key, i64::from(p.repeat_window_secs))
                .ignore()
                .sadd(&counted, fp)
                .ignore()
                .expire(&counted, RUN_TTL_SECS as i64)
                .ignore()
                .query_async(&mut conn)
                .await?;
        }
        if n >= u64::from(p.max_repeats) {
            trip = Some(Trip::Repeats(n));
        }
    }
    if seen.call {
        let key = keys.loop_key(seen.run, "calls");
        let (n, _): (u64, ()) = redis::pipe()
            .incr(&key, 1)
            .expire(&key, RUN_TTL_SECS as i64)
            .query_async(&mut conn)
            .await?;
        if p.max_calls_per_run.is_some_and(|max| n > u64::from(max)) {
            trip = trip.or(Some(Trip::Calls(n)));
        }
    }
    if seen.cost > 0
        && let Some(max) = p.max_usd_per_minute
    {
        let minute = chrono::Utc::now().timestamp() / 60;
        let key = keys.loop_key(seen.run, &format!("m:{minute}"));
        let (spent, _): (u64, ()) = redis::pipe()
            .incr(&key, seen.cost)
            .expire(&key, 180)
            .query_async(&mut conn)
            .await?;
        if spent as f64 > max * 1e6 {
            trip = trip.or(Some(Trip::Velocity(spent)));
        }
    }
    let Some(trip) = trip else { return Ok(None) };
    let detail = trip.describe(&p);
    let first: Option<String> = redis::cmd("SET")
        .arg(keys.loop_key(seen.run, "tripped"))
        .arg(&detail)
        .arg("NX")
        .arg("EX")
        .arg(RUN_TTL_SECS)
        .query_async(&mut conn)
        .await?;
    if first.is_none() {
        return Ok(None);
    }
    tripped(st, &seen, p.action, &detail).await?;
    Ok(matches!(p.action, LoopAction::Pause | LoopAction::Kill).then_some(p.action))
}

async fn tripped(st: &AppState, seen: &Seen<'_>, action: LoopAction, detail: &str) -> Result<()> {
    let verb = match action {
        LoopAction::Warn => "is still running",
        LoopAction::Pause => "was paused for approval",
        LoopAction::Kill => "was stopped",
    };
    let entry = Entry {
        org: Some(seen.org.to_string()),
        actor: "system".into(),
        action: "loop.detected".into(),
        target: seen.run.to_string(),
        detail: json!({ "detail": detail, "action": action }),
        ip: None,
    };
    audit::record(st, entry).await;
    let alert = Alert {
        event: AlertEvent::LoopDetected,
        severity: if action == LoopAction::Warn {
            Severity::Warning
        } else {
            Severity::Critical
        },
        title: format!("Run {} looks like a loop", seen.run),
        text: format!("{}/{}: {detail}. The run {verb}.", seen.project, seen.agent),
        dedup: format!("loop:{}", seen.run),
        quiet_secs: 3600,
        tab: Some(if action == LoopAction::Pause {
            "approvals"
        } else {
            "runs"
        }),
        fields: vec![
            ("Run".into(), seen.run.to_string()),
            ("Project".into(), seen.project.to_string()),
            ("Agent".into(), seen.agent.to_string()),
        ],
    };
    alerts::notify(st, seen.org, alert).await;
    match action {
        LoopAction::Warn => {}
        LoopAction::Pause => {
            let request = Request {
                org: seen.org,
                run: seen.run,
                project: seen.project,
                agent: seen.agent,
                reason: Reason::Loop { detail },
            };
            approvals::request(st, request).await?;
        }
        LoopAction::Kill => {
            st.ledger
                .set_run_state(seen.org, seen.run, "killed")
                .await?
        }
    }
    Ok(())
}

/// Let a run trip again after an approver allowed it to continue.
pub async fn reset(st: &AppState, org: &str, run: &str) -> Result<()> {
    let keys = Keys::new(org);
    let mut conn = st.ledger.conn();
    let counted = keys.loop_key(run, "fps");
    let fingerprints: Vec<String> = conn.smembers(&counted).await?;
    let mut stale = vec![keys.loop_key(run, "tripped"), keys.loop_key(run, "calls")];
    stale.extend(
        fingerprints
            .iter()
            .map(|fp| keys.loop_key(run, &format!("fp:{fp}"))),
    );
    stale.push(counted);
    let _: () = conn.del(&stale).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprints_follow_the_last_message() {
        let a = json!({"messages": [{"role": "user", "content": "hi"}, {"role": "user", "content": "again"}]});
        let b = json!({"messages": [{"role": "user", "content": "other"}, {"role": "user", "content": "again"}]});
        let c = json!({"messages": [{"role": "user", "content": "new"}]});
        let f = |m: &str, v: &Value| request_fingerprint(m, v).unwrap();
        assert_eq!(f("gpt", &a), f("gpt", &b));
        assert_ne!(f("gpt", &a), f("gpt", &c));
        assert_ne!(f("gpt", &a), f("claude", &a));
        assert!(request_fingerprint("gpt", &json!({"input": "hello"})).is_some());
        assert!(request_fingerprint("gpt", &json!({})).is_none());
        assert!(valid_fingerprint(&f("gpt", &a)));
        assert!(!valid_fingerprint("not-hex"));
    }
}
