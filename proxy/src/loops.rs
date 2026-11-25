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
    /// The model call's whole prompt in tokens, when known: how big the context has grown.
    pub prompt_tokens: u64,
    /// A tool result: the tool and its status ("ok", "error"). A changed status is new
    /// information, like a failing test that starts to pass.
    pub outcome: Option<(&'a str, &'a str)>,
}

/// Context growth with nothing new that counts as no progress (with
/// `max_calls_without_progress` calls in a row).
const NO_PROGRESS_GROWTH: u64 = 20_000;
/// How long a nudge waits for the agent's next request or hook before it is dropped.
const NUDGE_TTL_SECS: u64 = 3_600;

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
    NoProgress { calls: u64, growth: u64 },
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
            Trip::NoProgress { calls, growth } => format!(
                "{calls} model calls in a row added {} tokens of context but nothing new (no new \
                 tool call, no changed result)",
                thousands(*growth)
            ),
        }
    }

    /// What the agent is told when it is nudged instead of stopped.
    fn nudge(&self) -> String {
        let body = match self {
            Trip::Repeats(n) => format!(
                "You have made the same call {n} times in this run and it is not getting you \
                 closer. Stop repeating it: re-read the last error or output, then try a different \
                 approach, or stop and report what is blocking you."
            ),
            Trip::NoProgress { calls, growth } => format!(
                "Your last {calls} steps added about {} tokens of context without new information \
                 (no new files, tool calls or changed results). Summarise what you know, then try \
                 a different approach, or stop and report.",
                thousands(*growth)
            ),
            Trip::Velocity(_) | Trip::Calls(_) => "This run is spending fast and is close to its \
                 limit. Finish the current step, then wrap up and report."
                .to_string(),
        };
        format!(
            "[Tokenrein] {body} If this continues, the run will be stopped for a person to review."
        )
    }
}

fn thousands(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, ch) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

/// The note waiting for a nudged run's next request or hook, taken so it is delivered once.
pub async fn take_nudge(st: &AppState, org: &str, run: &str) -> Option<String> {
    let mut conn = st.ledger.conn();
    redis::cmd("GETDEL")
        .arg(Keys::new(org).loop_key(run, "nudge"))
        .query_async::<Option<String>>(&mut conn)
        .await
        .unwrap_or(None)
}

/// Record and announce a nudge (the run goes on).
async fn nudged(st: &AppState, seen: &Seen<'_>, detail: &str) {
    let entry = Entry {
        org: Some(seen.org.to_string()),
        actor: "system".into(),
        action: "loop.nudged".into(),
        target: seen.run.to_string(),
        detail: json!({ "detail": detail }),
        ip: None,
    };
    audit::record(st, entry).await;
    let alert = Alert {
        event: AlertEvent::LoopDetected,
        severity: Severity::Warning,
        title: format!("Run {} was nudged", seen.run),
        text: format!(
            "{}/{}: {detail}. The agent was told to change course; if it carries on, the run is \
             stopped.",
            seen.project, seen.agent
        ),
        dedup: format!("nudge:{}", seen.run),
        quiet_secs: 3600,
        tab: Some("runs"),
        fields: vec![
            ("Run".into(), seen.run.to_string()),
            ("Project".into(), seen.project.to_string()),
            ("Agent".into(), seen.agent.to_string()),
        ],
    };
    alerts::notify(st, seen.org, alert).await;
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
    // New information: a tool call the run has not made before, or a tool whose result changed.
    let mut new_info = false;
    if let Some(fp) = seen.fingerprint {
        let key = keys.loop_key(seen.run, &format!("fp:{fp}"));
        let counted = keys.loop_key(seen.run, "fps");
        let (n, added): (u64, u64) = redis::pipe()
            .incr(&key, 1)
            .sadd(&counted, fp)
            .expire(&counted, RUN_TTL_SECS as i64)
            .ignore()
            .query_async(&mut conn)
            .await?;
        if n == 1 {
            let _: () = conn.expire(&key, i64::from(p.repeat_window_secs)).await?;
        }
        new_info |= added == 1;
        if n >= u64::from(p.max_repeats) {
            trip = Some(Trip::Repeats(n));
        }
    }
    if let Some((tool, status)) = seen.outcome {
        let key = keys.loop_key(seen.run, "outcomes");
        let (before,): (Option<String>,) = redis::pipe()
            .hget(&key, tool)
            .hset(&key, tool, status)
            .ignore()
            .expire(&key, RUN_TTL_SECS as i64)
            .ignore()
            .query_async(&mut conn)
            .await?;
        new_info |= before.as_deref() != Some(status);
    }
    if let Some(max) = p.max_calls_without_progress {
        let key = keys.loop_key(seen.run, "progress");
        if new_info {
            let _: () = redis::pipe()
                .hset(&key, "calls", 0)
                .ignore()
                .hdel(&key, "base")
                .ignore()
                .expire(&key, RUN_TTL_SECS as i64)
                .ignore()
                .query_async(&mut conn)
                .await?;
        }
        if seen.call && seen.prompt_tokens > 0 {
            let (calls, base): (u64, u64) = redis::pipe()
                .hincr(&key, "calls", 1)
                .hset_nx(&key, "base", seen.prompt_tokens)
                .ignore()
                .hget(&key, "base")
                .expire(&key, RUN_TTL_SECS as i64)
                .ignore()
                .query_async(&mut conn)
                .await?;
            let growth = seen.prompt_tokens.saturating_sub(base);
            if calls > u64::from(max) && growth >= NO_PROGRESS_GROWTH {
                trip = trip.or(Some(Trip::NoProgress { calls, growth }));
            }
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
    // Nudge before stopping: the first trip tells the agent and lets it go on; the counts that
    // tripped start over, so only carrying on the same way trips it again.
    if p.nudge && p.action != LoopAction::Warn {
        let first_nudge: Option<String> = redis::cmd("SET")
            .arg(keys.loop_key(seen.run, "nudged"))
            .arg(&detail)
            .arg("NX")
            .arg("EX")
            .arg(RUN_TTL_SECS)
            .query_async(&mut conn)
            .await?;
        if first_nudge.is_some() {
            let mut stale = vec![keys.loop_key(seen.run, "progress")];
            if let (Trip::Repeats(_), Some(fp)) = (&trip, seen.fingerprint) {
                stale.push(keys.loop_key(seen.run, &format!("fp:{fp}")));
            }
            let _: () = redis::pipe()
                .del(&stale)
                .ignore()
                .set_ex(
                    keys.loop_key(seen.run, "nudge"),
                    trip.nudge(),
                    NUDGE_TTL_SECS,
                )
                .ignore()
                .query_async(&mut conn)
                .await?;
            nudged(st, &seen, &detail).await;
            return Ok(None);
        }
    }
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
    let mut stale = vec![
        keys.loop_key(run, "tripped"),
        keys.loop_key(run, "calls"),
        keys.loop_key(run, "progress"),
        keys.loop_key(run, "nudged"),
        keys.loop_key(run, "nudge"),
    ];
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
