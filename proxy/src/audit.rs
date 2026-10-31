//! The admin audit log: who changed what, and when.
//!
//! Every change made through the admin API is recorded with the person or token that made it
//! and, for config changes, exactly which fields changed (secrets redacted). Entries are
//! append-only. With a database they go to Postgres in the same transaction as the config
//! change; without one they go to a capped Redis stream. Each entry is also written to the log
//! (`target: "audit"`), so it can be shipped to a SIEM.

use serde::Serialize;
use serde_json::{Map, Value, json};

use crate::proxy::AppState;

/// Most changes listed for one config edit; the rest are summarised.
const MAX_CHANGES: usize = 200;

#[derive(Debug, Clone, Serialize)]
pub struct Entry {
    /// The org the change applies to; None for instance-wide changes (model prices).
    pub org: Option<String>,
    pub actor: String,
    pub action: String,
    pub target: String,
    pub detail: Value,
    pub ip: Option<String>,
}

/// A config edit about to be made, for the audit log.
#[derive(Debug, Clone)]
pub struct Change {
    pub actor: String,
    pub ip: Option<String>,
    pub org: Option<String>,
    pub action: &'static str,
    pub target: String,
}

impl Change {
    pub fn entry(&self, detail: Value) -> Entry {
        Entry {
            org: self.org.clone(),
            actor: self.actor.clone(),
            action: self.action.to_string(),
            target: self.target.clone(),
            detail,
            ip: self.ip.clone(),
        }
    }
}

/// Field-level changes between two JSON documents, as `{path, before, after}` objects.
/// Lists of objects are matched by their `id` (or `hash`, or `provider` + `model`), so adding a
/// key shows up as one added key rather than a changed list. Sealed secrets are redacted.
pub fn diff(before: &Value, after: &Value) -> Vec<Value> {
    let mut out = Vec::new();
    walk("", &redact(before), &redact(after), &mut out);
    if out.len() > MAX_CHANGES {
        let n = out.len() - MAX_CHANGES;
        out.truncate(MAX_CHANGES);
        out.push(json!({ "path": "…", "note": format!("{n} more changes not shown") }));
    }
    out
}

fn walk(path: &str, a: &Value, b: &Value, out: &mut Vec<Value>) {
    if a == b {
        return;
    }
    match (a, b) {
        (Value::Object(x), Value::Object(y)) => {
            let mut keys: Vec<&String> = x.keys().chain(y.keys()).collect();
            keys.sort();
            keys.dedup();
            for k in keys {
                let p = if path.is_empty() {
                    k.clone()
                } else {
                    format!("{path}.{k}")
                };
                walk(
                    &p,
                    x.get(k).unwrap_or(&Value::Null),
                    y.get(k).unwrap_or(&Value::Null),
                    out,
                );
            }
        }
        // Empty lists are left out of the stored document; compare them as empty.
        (Value::Null, Value::Array(_)) => walk(path, &Value::Array(Vec::new()), b, out),
        (Value::Array(_), Value::Null) => walk(path, a, &Value::Array(Vec::new()), out),
        (Value::Array(x), Value::Array(y)) if keyed(x) && keyed(y) => {
            let find = |list: &Vec<Value>, k: &str| list.iter().find(|v| key_of(v) == k).cloned();
            let keys: Vec<String> = x.iter().chain(y.iter()).map(key_of).collect();
            let mut seen = std::collections::HashSet::new();
            for k in keys {
                if !seen.insert(k.clone()) {
                    continue;
                }
                walk(
                    &format!("{path}[{k}]"),
                    &find(x, &k).unwrap_or(Value::Null),
                    &find(y, &k).unwrap_or(Value::Null),
                    out,
                );
            }
        }
        _ => out.push(json!({ "path": path, "before": a, "after": b })),
    }
}

/// Lists whose items all have a key (an empty list counts, so removing the last item works).
fn keyed(list: &[Value]) -> bool {
    list.iter().all(|v| !key_of(v).is_empty())
}

fn key_of(v: &Value) -> String {
    let s = |k: &str| v.get(k).and_then(Value::as_str).unwrap_or("");
    if !s("id").is_empty() {
        return s("id").to_string();
    }
    if !s("hash").is_empty() {
        // Key hashes are long; the dashboard shows keys by their first 12 characters.
        return s("hash").chars().take(12).collect();
    }
    if !s("provider").is_empty() && !s("model").is_empty() {
        return format!("{}/{}", s("provider"), s("model"));
    }
    String::new()
}

/// Replace sealed secrets with a marker; the last 4 characters stay so changes are visible.
fn redact(v: &Value) -> Value {
    match v {
        Value::Object(m) => Value::Object(
            m.iter()
                .map(|(k, v)| {
                    // Sealed provider keys, and the license key (a bearer credential).
                    let v = if k == "sealed" || k.ends_with("_sealed") || k == "license" {
                        Value::String("[redacted]".into())
                    } else {
                        redact(v)
                    };
                    (k.clone(), v)
                })
                .collect::<Map<_, _>>(),
        ),
        Value::Array(a) => Value::Array(a.iter().map(redact).collect()),
        other => other.clone(),
    }
}

fn log(e: &Entry, config_version: Option<u64>) {
    tracing::info!(
        target: "audit",
        org = e.org.as_deref().unwrap_or(""),
        actor = %e.actor,
        action = %e.action,
        target_id = %e.target,
        ip = e.ip.as_deref().unwrap_or(""),
        config_version,
        "audit"
    );
}

/// Record an entry that is not part of a config change (pausing a run, testing a key, ...).
/// Failures are logged, not returned: the action itself already happened.
pub async fn record(state: &AppState, e: Entry) {
    log(&e, None);
    let result = match &state.db {
        Some(db) => db.audit(&e).await,
        None => state.ledger.audit_append(&e, None).await,
    };
    if let Err(err) = result {
        tracing::error!(error = %err, action = %e.action, "could not write the audit log");
        crate::metrics::audit_write_failed();
    }
}

/// Log an entry the config store has saved along with config `config_version`.
pub(crate) fn logged(e: &Entry, config_version: u64) {
    log(e, Some(config_version));
}

/// Newest first. `before` is the `next` cursor of a previous page.
pub async fn list(
    state: &AppState,
    org: Option<&str>,
    before: Option<&str>,
    limit: usize,
) -> anyhow::Result<(Vec<Value>, Option<String>)> {
    let limit = limit.clamp(1, 500);
    let rows = match &state.db {
        Some(db) => {
            let before = before.and_then(|b| b.parse::<i64>().ok());
            db.audit_list(org, before, limit).await?
        }
        None => state.ledger.audit_list(org, before, limit).await?,
    };
    let next = (rows.len() == limit)
        .then(|| {
            rows.last()
                .and_then(|r| r["id"].as_str())
                .map(str::to_string)
        })
        .flatten();
    Ok((rows, next))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths(d: &[Value]) -> Vec<&str> {
        d.iter().map(|c| c["path"].as_str().unwrap()).collect()
    }

    #[test]
    fn diff_matches_list_items_by_id() {
        let a = json!({"orgs": [{"id": "acme", "projects": [{"id": "bot", "budgets": {"run": {"limit_usd": 1.0}}}]}]});
        let b = json!({"orgs": [{"id": "acme", "projects": [
            {"id": "bot", "budgets": {"run": {"limit_usd": 2.0}}},
            {"id": "new", "budgets": {}}
        ]}]});
        let d = diff(&a, &b);
        assert_eq!(
            paths(&d),
            [
                "orgs[acme].projects[bot].budgets.run.limit_usd",
                "orgs[acme].projects[new]"
            ]
        );
        assert_eq!(d[0]["before"], 1.0);
        assert_eq!(d[0]["after"], 2.0);
        assert_eq!(d[1]["before"], Value::Null);
    }

    #[test]
    fn diff_redacts_secrets_and_keys_by_hash() {
        let a = json!({"keys": [{"hash": "aaaaaaaaaaaaaaaaaaaa", "name": "one"}],
                       "provider_secrets": {"openai": {"sealed": "old-cipher", "last4": "1111"}}});
        let b = json!({"keys": [], "provider_secrets": {"openai": {"sealed": "new-cipher", "last4": "2222"}}});
        let d = diff(&a, &b);
        let text = serde_json::to_string(&d).unwrap();
        assert!(!text.contains("cipher"), "{text}");
        // The sealed value looks unchanged after redaction; last4 shows the key changed.
        assert_eq!(
            paths(&d),
            ["keys[aaaaaaaaaaaa]", "provider_secrets.openai.last4"]
        );
        assert_eq!(d[0]["after"], Value::Null);
    }

    #[test]
    fn diff_of_equal_documents_is_empty() {
        let a = json!({"x": [1, 2], "y": {"z": null}});
        assert!(diff(&a, &a).is_empty());
    }
}
