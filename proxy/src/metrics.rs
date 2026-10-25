//! Prometheus metrics, served as text from `GET /metrics` on the admin listener.
//!
//! Hand-rolled atomics with fixed label sets: nothing here allocates or takes a lock on the
//! request path, and no org, project or user ids are exported. Counters are process-wide (one
//! set per proxy process).

use std::fmt::Write as _;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use axum::extract::State;
use axum::http::HeaderValue;
use axum::http::header::CONTENT_TYPE;
use axum::response::{IntoResponse, Response};
use serde_json::Value;
use tokio::sync::mpsc;

use crate::proxy::AppState;

/// Where a decision was made: in the request path, or for usage reported to the ingest API.
#[derive(Clone, Copy)]
pub enum Path {
    Proxy = 0,
    Ingest = 1,
}
const PATHS: [&str; 2] = ["proxy", "ingest"];

/// Decision values as they appear in request events; anything else is counted as "other".
const DECISIONS: [&str; 7] = [
    "allowed",
    "allowed_degraded",
    "blocked_budget",
    "blocked_run_stopped",
    "upstream_error",
    // Refused because the ledger was unavailable and the org fails closed.
    "unavailable",
    "other",
];

const PROVIDERS: [&str; 3] = ["openai", "anthropic", "other"];

/// Seconds. Pre-upstream overhead is the time Tokenrein adds before the upstream call.
const OVERHEAD_BUCKETS: &[f64] = &[
    0.0001, 0.00025, 0.0005, 0.001, 0.0025, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25,
];
const UPSTREAM_BUCKETS: &[f64] = &[
    0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0, 60.0, 120.0, 300.0, 600.0,
];
const MAX_BUCKETS: usize = 16;

struct Histogram {
    bounds: &'static [f64],
    /// Non-cumulative counts per bucket; the slot after the last bound is +Inf.
    buckets: [AtomicU64; MAX_BUCKETS + 1],
    sum_micros: AtomicU64,
    count: AtomicU64,
}

impl Histogram {
    const fn new(bounds: &'static [f64]) -> Self {
        assert!(bounds.len() <= MAX_BUCKETS);
        Self {
            bounds,
            buckets: [const { AtomicU64::new(0) }; MAX_BUCKETS + 1],
            sum_micros: AtomicU64::new(0),
            count: AtomicU64::new(0),
        }
    }

    fn observe_micros(&self, micros: u64) {
        let secs = micros as f64 / 1e6;
        let i = self
            .bounds
            .iter()
            .position(|b| secs <= *b)
            .unwrap_or(self.bounds.len());
        self.buckets[i].fetch_add(1, Ordering::Relaxed);
        self.sum_micros.fetch_add(micros, Ordering::Relaxed);
        self.count.fetch_add(1, Ordering::Relaxed);
    }

    fn render(&self, out: &mut String, name: &str, labels: &str) {
        let sep = if labels.is_empty() { "" } else { "," };
        let mut cumulative = 0;
        for (i, b) in self.bounds.iter().enumerate() {
            cumulative += self.buckets[i].load(Ordering::Relaxed);
            let _ = writeln!(out, "{name}_bucket{{{labels}{sep}le=\"{b}\"}} {cumulative}");
        }
        cumulative += self.buckets[self.bounds.len()].load(Ordering::Relaxed);
        let _ = writeln!(
            out,
            "{name}_bucket{{{labels}{sep}le=\"+Inf\"}} {cumulative}"
        );
        let braces = if labels.is_empty() {
            String::new()
        } else {
            format!("{{{labels}}}")
        };
        let sum = self.sum_micros.load(Ordering::Relaxed) as f64 / 1e6;
        let count = self.count.load(Ordering::Relaxed);
        let _ = writeln!(
            out,
            "{name}_sum{braces} {sum}\n{name}_count{braces} {count}"
        );
    }
}

struct Metrics {
    decisions: [[AtomicU64; DECISIONS.len()]; PATHS.len()],
    overhead: Histogram,
    upstream: [Histogram; PROVIDERS.len()],
    ledger_errors: AtomicU64,
    activity_dropped_queue_full: AtomicU64,
    activity_dropped_write_failed: AtomicU64,
    audit_write_failures: AtomicU64,
}

static METRICS: Metrics = Metrics {
    decisions: [const { [const { AtomicU64::new(0) }; DECISIONS.len()] }; PATHS.len()],
    overhead: Histogram::new(OVERHEAD_BUCKETS),
    upstream: [const { Histogram::new(UPSTREAM_BUCKETS) }; PROVIDERS.len()],
    ledger_errors: AtomicU64::new(0),
    activity_dropped_queue_full: AtomicU64::new(0),
    activity_dropped_write_failed: AtomicU64::new(0),
    audit_write_failures: AtomicU64::new(0),
};

pub fn decision(path: Path, decision: &str) {
    let i = DECISIONS
        .iter()
        .position(|d| *d == decision)
        .unwrap_or(DECISIONS.len() - 1);
    METRICS.decisions[path as usize][i].fetch_add(1, Ordering::Relaxed);
}

/// Time spent before the upstream call (auth, pricing, budget reservation).
pub fn pre_upstream(micros: u64) {
    METRICS.overhead.observe_micros(micros);
}

pub fn upstream(provider: &str, millis: u64) {
    let i = PROVIDERS
        .iter()
        .position(|p| *p == provider)
        .unwrap_or(PROVIDERS.len() - 1);
    METRICS.upstream[i].observe_micros(millis.saturating_mul(1000));
}

/// A budget ledger (Redis) call failed.
pub fn ledger_error() {
    METRICS.ledger_errors.fetch_add(1, Ordering::Relaxed);
}

/// Dashboard activity events that could not be stored in Redis.
pub fn activity_write_failed(events: usize) {
    METRICS
        .activity_dropped_write_failed
        .fetch_add(events as u64, Ordering::Relaxed);
}

pub fn audit_write_failed() {
    METRICS.audit_write_failures.fetch_add(1, Ordering::Relaxed);
}

/// Queue a request event for the dashboard and count its decision. Best effort: when the queue
/// is full the event is dropped (and counted), never waited for.
pub fn publish(tx: &mpsc::Sender<Value>, path: Path, event: Value) {
    if let Some(d) = event.get("decision").and_then(Value::as_str) {
        decision(path, d);
    }
    if tx.try_send(event).is_err() {
        METRICS
            .activity_dropped_queue_full
            .fetch_add(1, Ordering::Relaxed);
    }
}

fn header(out: &mut String, name: &str, kind: &str, help: &str) {
    let _ = writeln!(out, "# HELP {name} {help}\n# TYPE {name} {kind}");
}

pub fn render(config_version: u64) -> String {
    let m = &METRICS;
    let load = |a: &AtomicU64| a.load(Ordering::Relaxed);
    let mut out = String::with_capacity(8 * 1024);

    header(
        &mut out,
        "tokenrein_build_info",
        "gauge",
        "Tokenrein proxy version.",
    );
    let _ = writeln!(
        out,
        "tokenrein_build_info{{version=\"{}\"}} 1",
        env!("CARGO_PKG_VERSION")
    );

    header(
        &mut out,
        "tokenrein_decisions_total",
        "counter",
        "Requests by guardrail decision.",
    );
    for (p, path) in PATHS.iter().enumerate() {
        for (d, decision) in DECISIONS.iter().enumerate() {
            let _ = writeln!(
                out,
                "tokenrein_decisions_total{{path=\"{path}\",decision=\"{decision}\"}} {}",
                load(&m.decisions[p][d])
            );
        }
    }

    header(
        &mut out,
        "tokenrein_pre_upstream_seconds",
        "histogram",
        "Time Tokenrein adds before calling the provider.",
    );
    m.overhead
        .render(&mut out, "tokenrein_pre_upstream_seconds", "");

    header(
        &mut out,
        "tokenrein_upstream_seconds",
        "histogram",
        "Provider response time, to the end of the body.",
    );
    for (i, provider) in PROVIDERS.iter().enumerate() {
        m.upstream[i].render(
            &mut out,
            "tokenrein_upstream_seconds",
            &format!("provider=\"{provider}\""),
        );
    }

    header(
        &mut out,
        "tokenrein_ledger_errors_total",
        "counter",
        "Budget ledger (Redis) calls that failed.",
    );
    let _ = writeln!(
        out,
        "tokenrein_ledger_errors_total {}",
        load(&m.ledger_errors)
    );

    header(
        &mut out,
        "tokenrein_activity_events_dropped_total",
        "counter",
        "Dashboard activity events that were dropped.",
    );
    let _ = writeln!(
        out,
        "tokenrein_activity_events_dropped_total{{reason=\"queue_full\"}} {}\n\
         tokenrein_activity_events_dropped_total{{reason=\"write_failed\"}} {}",
        load(&m.activity_dropped_queue_full),
        load(&m.activity_dropped_write_failed)
    );

    header(
        &mut out,
        "tokenrein_audit_write_failures_total",
        "counter",
        "Audit log entries that could not be written.",
    );
    let _ = writeln!(
        out,
        "tokenrein_audit_write_failures_total {}",
        load(&m.audit_write_failures)
    );

    header(
        &mut out,
        "tokenrein_config_version",
        "gauge",
        "Version of the managed config this process is serving.",
    );
    let _ = writeln!(out, "tokenrein_config_version {config_version}");
    out
}

/// `GET /metrics`. Unauthenticated like `/healthz`: it holds counts only, no tenant ids.
pub async fn handler(State(st): State<Arc<AppState>>) -> Response {
    let version = st.control_version.load(Ordering::SeqCst);
    (
        [(
            CONTENT_TYPE,
            HeaderValue::from_static("text/plain; version=0.0.4; charset=utf-8"),
        )],
        render(version),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn histogram_buckets_are_cumulative() {
        let h = Histogram::new(&[0.001, 0.01]);
        h.observe_micros(500);
        h.observe_micros(5_000);
        h.observe_micros(50_000);
        let mut out = String::new();
        h.render(&mut out, "x", "p=\"a\"");
        assert!(out.contains("x_bucket{p=\"a\",le=\"0.001\"} 1\n"));
        assert!(out.contains("x_bucket{p=\"a\",le=\"0.01\"} 2\n"));
        assert!(out.contains("x_bucket{p=\"a\",le=\"+Inf\"} 3\n"));
        assert!(out.contains("x_sum{p=\"a\"} 0.0555\n"));
        assert!(out.contains("x_count{p=\"a\"} 3\n"));
    }

    #[test]
    fn unknown_labels_fall_into_other() {
        decision(Path::Ingest, "something_new");
        upstream("some-new-provider", 10);
        let out = render(7);
        let other = out
            .lines()
            .find(|l| {
                l.starts_with("tokenrein_decisions_total{path=\"ingest\",decision=\"other\"}")
            })
            .unwrap();
        assert!(!other.ends_with(" 0"));
        assert!(!out.contains("tokenrein_upstream_seconds_count{provider=\"other\"} 0\n"));
        assert!(out.contains("tokenrein_config_version 7\n"));
    }
}
