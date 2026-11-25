//! Run cost forecast: where a run ends up if it carries on the way it is going.
//!
//! An agent re-sends its whole context on every call, and the context grows with every step, so
//! each call tends to cost a little more than the one before: per-call cost grows roughly
//! linearly, and the run's total roughly with the square of its steps. A least-squares line
//! through the run's per-call costs (kept as running sums, so each update is O(1)) projects the
//! next calls. That turns "this run has spent $5" into "this run passes its $60 budget in about
//! 9 more calls", early enough to act on.

use redis::AsyncCommands;
use serde::Serialize;

use crate::alerts::{self, Alert, Severity};
use crate::config::AlertEvent;
use crate::ledger::{Keys, RUN_TTL_SECS};
use crate::proxy::AppState;

/// Calls needed before a forecast means anything.
const MIN_CALLS: u64 = 5;
/// Furthest a forecast looks ahead for the budget crossing.
const MAX_LOOKAHEAD: u64 = 10_000;
/// Without a run budget, warn when going on as long again would cost at least this (micro-USD).
const WARN_WITHOUT_LIMIT: u64 = 5_000_000;

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Forecast {
    /// Model calls so far, and what they cost.
    pub calls: u64,
    pub spent_micros: u64,
    /// The fitted cost of the latest call, and how much more each call costs than the last.
    pub per_call_micros: u64,
    pub growth_per_call_micros: i64,
    /// What the run will have cost if it makes as many calls again.
    pub as_long_again_micros: u64,
    /// The run's budget, and how many more calls until it is used up at this rate.
    pub limit_micros: Option<u64>,
    pub calls_to_limit: Option<u64>,
}

/// Least-squares line `cost = a + b * x` through the calls `x = 1..=n`, from `sum(cost)` and
/// `sum(x * cost)`.
fn fit(n: u64, sy: f64, sxy: f64) -> (f64, f64) {
    let n_f = n as f64;
    let sx = n_f * (n_f + 1.0) / 2.0;
    let sxx = n_f * (n_f + 1.0) * (2.0 * n_f + 1.0) / 6.0;
    let den = n_f * sxx - sx * sx;
    let b = if den > 0.0 {
        (n_f * sxy - sx * sy) / den
    } else {
        0.0
    };
    let a = (sy - b * sx) / n_f;
    (a, b)
}

/// The cost of calls `n+1 ..= n+m` on the line (a call never costs less than nothing).
fn next_calls(n: u64, a: f64, b: f64, m: u64) -> f64 {
    (n + 1..=n + m).map(|x| (a + b * x as f64).max(0.0)).sum()
}

/// The forecast for a run whose `n` tracked calls cost `sy` micro-USD (`sxy` = sum of call
/// number times cost), with `spent` the run's whole spend so far (it may predate tracking) and
/// `limit` its budget; `None` until there are enough calls.
pub fn compute(n: u64, sy: u64, sxy: u64, spent: u64, limit: Option<u64>) -> Option<Forecast> {
    if n < MIN_CALLS {
        return None;
    }
    let (a, b) = fit(n, sy as f64, sxy as f64);
    let as_long_again = spent + next_calls(n, a, b, n).round() as u64;
    let calls_to_limit = limit.filter(|l| *l > spent).and_then(|l| {
        let need = (l - spent) as f64;
        let mut total = 0.0;
        for m in 1..=MAX_LOOKAHEAD {
            total += (a + b * (n + m) as f64).max(0.0);
            if total >= need {
                return Some(m);
            }
        }
        None
    });
    Some(Forecast {
        calls: n,
        spent_micros: spent,
        per_call_micros: (a + b * n as f64).max(0.0).round() as u64,
        growth_per_call_micros: b.round() as i64,
        as_long_again_micros: as_long_again,
        limit_micros: limit,
        calls_to_limit,
    })
}

/// Whether a forecast is worth a warning: the run passes its budget within as many calls as it
/// has made so far while it has spent under half of it, or (with no budget) going on as long
/// again would cost at least $5 and three times what it has spent (a flat run only doubles).
pub fn worth_warning(f: &Forecast) -> bool {
    match (f.limit_micros, f.calls_to_limit) {
        (Some(l), Some(m)) => m <= f.calls.max(MIN_CALLS) && f.spent_micros * 2 < l,
        (Some(_), None) => false,
        (None, _) => {
            f.as_long_again_micros >= WARN_WITHOUT_LIMIT
                && f.as_long_again_micros >= f.spent_micros.saturating_mul(3)
        }
    }
}

fn usd(m: u64) -> String {
    format!("${:.2}", m as f64 / 1e6)
}

/// The run's spend and budget limit in micro-USD, from the ledger.
async fn run_budget(st: &AppState, org: &str, run: &str) -> (u64, Option<u64>) {
    let Ok(b) = st.ledger.budget(&Keys::new(org).run_budget(run)).await else {
        return (0, None);
    };
    let num = |k: &str| b.get(k).and_then(|v| v.parse::<f64>().ok());
    (
        num("spent").unwrap_or(0.0).max(0.0) as u64,
        num("limit").filter(|l| *l > 0.0).map(|l| l as u64),
    )
}

/// The run's forecast now, if it has enough calls.
pub async fn get(st: &AppState, org: &str, run: &str) -> Option<Forecast> {
    let mut conn = st.ledger.conn();
    let h: std::collections::HashMap<String, u64> = conn
        .hgetall(Keys::new(org).loop_key(run, "fc"))
        .await
        .ok()?;
    let v = |k: &str| h.get(k).copied().unwrap_or(0);
    let (spent, limit) = run_budget(st, org, run).await;
    compute(v("n"), v("sy"), v("sxy"), spent.max(v("sy")), limit)
}

/// Where a call happened, for [`record`].
pub struct Call<'a> {
    pub org: &'a str,
    pub project: &'a str,
    pub agent: &'a str,
    pub run: &'a str,
    pub cost: u64,
}

/// Count a finished model call and warn once per run when its forecast is worth it. Best effort.
pub async fn record(st: &AppState, c: &Call<'_>) -> Option<Forecast> {
    if c.cost == 0 {
        return None;
    }
    let keys = Keys::new(c.org);
    let key = keys.loop_key(c.run, "fc");
    let mut conn = st.ledger.conn();
    let x: u64 = conn.hincr(&key, "n", 1).await.ok()?;
    let (sy, sxy): (u64, u64) = redis::pipe()
        .hincr(&key, "sy", c.cost)
        .hincr(&key, "sxy", x.saturating_mul(c.cost))
        .expire(&key, RUN_TTL_SECS as i64)
        .ignore()
        .query_async(&mut conn)
        .await
        .ok()?;
    let (spent, limit) = run_budget(st, c.org, c.run).await;
    let f = compute(x, sy, sxy, spent.max(sy), limit)?;
    if worth_warning(&f) {
        let first: Option<String> = redis::cmd("SET")
            .arg(keys.loop_key(c.run, "fcw"))
            .arg(1)
            .arg("NX")
            .arg("EX")
            .arg(RUN_TTL_SECS)
            .query_async(&mut conn)
            .await
            .unwrap_or(None);
        if first.is_some() {
            warn(st, c, &f).await;
        }
    }
    Some(f)
}

async fn warn(st: &AppState, c: &Call<'_>, f: &Forecast) {
    let growth = if f.growth_per_call_micros > 0 {
        format!(
            "each call costs about {} more than the one before",
            usd(f.growth_per_call_micros as u64)
        )
    } else {
        format!("calls cost about {} each", usd(f.per_call_micros))
    };
    let (title, text) = match (f.limit_micros, f.calls_to_limit) {
        (Some(l), Some(m)) => (
            format!("Run {} is on course to pass its {} budget", c.run, usd(l)),
            format!(
                "{}/{} has spent {} in {} calls and {growth}: at this rate it uses up its {} run \
                 budget in about {m} more calls.",
                c.project,
                c.agent,
                usd(f.spent_micros),
                f.calls,
                usd(l)
            ),
        ),
        _ => (
            format!(
                "Run {} is on course for {}",
                c.run,
                usd(f.as_long_again_micros)
            ),
            format!(
                "{}/{} has spent {} in {} calls and {growth}: if it goes on as long again it will \
                 have cost about {}.",
                c.project,
                c.agent,
                usd(f.spent_micros),
                f.calls,
                usd(f.as_long_again_micros)
            ),
        ),
    };
    let alert = Alert {
        event: AlertEvent::BudgetWarning,
        severity: Severity::Warning,
        title,
        text,
        dedup: format!("forecast:{}", c.run),
        quiet_secs: 3600,
        tab: Some("runs"),
        fields: vec![
            ("Run".into(), c.run.to_string()),
            ("Spent".into(), usd(f.spent_micros)),
            ("Forecast".into(), usd(f.as_long_again_micros)),
        ],
    };
    alerts::notify(st, c.org, alert).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Running sums for per-call costs.
    fn sums(costs: &[u64]) -> (u64, u64, u64) {
        let n = costs.len() as u64;
        let sy = costs.iter().sum();
        let sxy = costs
            .iter()
            .enumerate()
            .map(|(i, c)| (i as u64 + 1) * c)
            .sum();
        (n, sy, sxy)
    }

    #[test]
    fn growing_context_projects_quadratic_spend() {
        // Each call costs $0.10 more than the last: $0.10, $0.20, ... $1.00.
        let costs: Vec<u64> = (1..=10).map(|i| i * 100_000).collect();
        let (n, sy, sxy) = sums(&costs);
        let f = compute(n, sy, sxy, sy, None).unwrap();
        assert_eq!(f.spent_micros, 5_500_000);
        assert_eq!(f.growth_per_call_micros, 100_000);
        assert_eq!(f.per_call_micros, 1_000_000);
        // Ten more calls cost $1.10 ... $2.00 = $15.50, on top of $5.50.
        assert_eq!(f.as_long_again_micros, 21_000_000);
        assert!(worth_warning(&f), "{f:?}");
        // With a $60 budget, $54.50 to go: crossed exactly where the cumulative cost passes it.
        let f = compute(n, sy, sxy, sy, Some(60_000_000)).unwrap();
        let m = f.calls_to_limit.unwrap();
        let spent_by = |m: u64| (11..=10 + m).map(|i| i * 100_000).sum::<u64>();
        assert!(
            spent_by(m) >= 54_500_000 && spent_by(m - 1) < 54_500_000,
            "{m}"
        );
        // Further away than the run's length so far: not yet worth a warning; closer, it is.
        assert!(m > 10 && !worth_warning(&f));
        let f = compute(n, sy, sxy, sy, Some(15_000_000)).unwrap();
        assert!(
            f.calls_to_limit.unwrap() <= 10 && worth_warning(&f),
            "{f:?}"
        );
    }

    #[test]
    fn flat_or_short_runs_are_left_alone() {
        let (n, sy, sxy) = sums(&[200_000; 10]);
        let f = compute(n, sy, sxy, sy, None).unwrap();
        assert_eq!(f.growth_per_call_micros, 0);
        assert_eq!(f.as_long_again_micros, 4_000_000);
        // As long again only doubles a flat run's spend: not worth a warning.
        assert!(!worth_warning(&f));
        assert!(compute(4, 100, 200, 100, None).is_none());
        // A run that spent $70 before tracking began is already past a $10 budget: no
        // countdown, and the projection starts from what it really spent.
        let f = compute(n, sy, sxy, 70_000_000, Some(10_000_000)).unwrap();
        assert_eq!(f.calls_to_limit, None);
        assert_eq!(f.as_long_again_micros, 72_000_000);
        assert!(!worth_warning(&f));
        // Already over half its budget: the budget itself takes over from here.
        let (n, sy, sxy) = sums(&(1..=10).map(|i| i * 100_000).collect::<Vec<_>>());
        assert!(!worth_warning(
            &compute(n, sy, sxy, sy, Some(8_000_000)).unwrap()
        ));
    }
}
