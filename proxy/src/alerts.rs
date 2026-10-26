//! Alerts to Slack, Microsoft Teams, PagerDuty and signed webhooks.
//!
//! Each org lists its channels (managed config, so changes are versioned and audited) and which
//! events each one wants. Sending never blocks a request: [`notify`] checks the alert's quiet
//! period, then delivers in the background with retries and records the last result per channel
//! for the dashboard. Events: a budget passed 80% ([`spawn_budget_watch`]), a request was refused
//! because a budget is used up (from the activity stream), a run waits for approval, a run looks
//! like a loop.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use chrono::{SecondsFormat, Utc};
use hmac::{Hmac, Mac};
use redis::AsyncCommands;
use serde_json::{Map, Value, json};
use sha2::Sha256;

use crate::config::{AlertChannel, AlertEvent, AlertKind};
use crate::ledger::Keys;
use crate::proxy::AppState;

const PAGERDUTY_URL: &str = "https://events.pagerduty.com/v2/enqueue";
/// Budgets at or over this share of their limit get a warning.
const WARN_AT: f64 = 0.8;
const ATTEMPTS: u32 = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Info,
    Warning,
    Critical,
}

impl Severity {
    fn as_str(self) -> &'static str {
        match self {
            Self::Info => "info",
            Self::Warning => "warning",
            Self::Critical => "critical",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Alert {
    pub event: AlertEvent,
    pub severity: Severity,
    pub title: String,
    pub text: String,
    /// Alerts with the same key are sent once per quiet period.
    pub dedup: String,
    pub quiet_secs: u64,
    /// Dashboard page the alert links to.
    pub tab: Option<&'static str>,
    pub fields: Vec<(String, String)>,
}

pub fn event_name(e: AlertEvent) -> &'static str {
    match e {
        AlertEvent::BudgetWarning => "budget_warning",
        AlertEvent::BudgetBlocked => "budget_blocked",
        AlertEvent::ApprovalRequested => "approval_requested",
        AlertEvent::LoopDetected => "loop_detected",
    }
}

/// The secret part of a channel (URL or routing key), sealed for one org and channel.
pub fn context(org: &str, channel: &str) -> String {
    format!("alert:{org}:{channel}")
}

fn signing_context(org: &str, channel: &str) -> String {
    format!("alert-signing:{org}:{channel}")
}

/// A channel ready to send: its kind, target and signing secret in the clear.
#[derive(Clone)]
struct Target {
    id: String,
    kind: AlertKind,
    url_or_key: String,
    signing: Option<String>,
}

fn pagerduty_url(st: &AppState) -> String {
    st.rt()
        .env_var("TOKENREIN_PAGERDUTY_URL")
        .unwrap_or_else(|| PAGERDUTY_URL.to_string())
}

fn link(st: &AppState, tab: Option<&str>) -> Option<String> {
    let rt = st.rt();
    rt.cfg.auth.public_url.as_deref().map(|u| {
        let u = u.trim_end_matches('/');
        match tab {
            Some(tab) => format!("{u}/#tab={tab}"),
            None => format!("{u}/"),
        }
    })
}

/// Send `alert` to the org's channels that want it, unless it was sent within its quiet period.
pub async fn notify(st: &AppState, org: &str, alert: Alert) {
    let rt = st.rt();
    let Some(o) = rt.cfg.orgs.iter().find(|o| o.id == org) else {
        return;
    };
    let wanted: Vec<&AlertChannel> = o
        .alert_channels
        .iter()
        .filter(|c| c.events.is_empty() || c.events.contains(&alert.event))
        .collect();
    if wanted.is_empty() {
        return;
    }
    let mut conn = st.ledger.conn();
    let first: Result<Option<String>, _> = redis::cmd("SET")
        .arg(Keys::new(org).alert_sent(&alert.dedup))
        .arg(1)
        .arg("NX")
        .arg("EX")
        .arg(alert.quiet_secs.max(1))
        .query_async(&mut conn)
        .await;
    match first {
        Ok(Some(_)) => {}
        Ok(None) => return,
        // Sending twice is better than not at all.
        Err(e) => tracing::warn!(error = %e, "alert quiet-period check failed; sending anyway"),
    }
    let Some(secrets) = rt.secrets() else {
        tracing::warn!(
            org,
            "alert channels are set but the secrets key is not; nothing sent"
        );
        return;
    };
    let targets: Vec<Target> = wanted
        .iter()
        .filter_map(|c| {
            let url_or_key = secrets.open(&context(org, &c.id), &c.sealed).ok()?;
            let signing = c
                .signing_sealed
                .as_ref()
                .and_then(|s| secrets.open(&signing_context(org, &c.id), s).ok());
            Some(Target {
                id: c.id.clone(),
                kind: c.kind,
                url_or_key,
                signing,
            })
        })
        .collect();
    let link = link(st, alert.tab);
    let pagerduty = pagerduty_url(st);
    let http = st.http.clone();
    let org = org.to_string();
    tokio::spawn(async move {
        for t in targets {
            let result = deliver(&http, &pagerduty, &org, &t, &alert, link.as_deref()).await;
            if let Err(e) = &result {
                tracing::warn!(org = %org, channel = %t.id, error = %e, "alert not delivered");
            }
            record_status(&mut conn, &org, &t.id, result.err()).await;
        }
    });
}

async fn record_status(
    conn: &mut redis::aio::ConnectionManager,
    org: &str,
    channel: &str,
    error: Option<String>,
) {
    let at = Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true);
    let ok = if error.is_none() { "true" } else { "false" };
    let fields = [
        ("at", at.as_str()),
        ("ok", ok),
        ("error", error.as_deref().unwrap_or("")),
    ];
    let r: Result<(), _> = conn
        .hset_multiple(Keys::new(org).alert_status(channel), &fields)
        .await;
    if let Err(e) = r {
        tracing::warn!(error = %e, "could not record alert delivery");
    }
}

/// The message body for a channel kind.
fn payload(t: &Target, org: &str, a: &Alert, link: Option<&str>) -> Value {
    let facts: Vec<(String, String)> = std::iter::once(("Org".to_string(), org.to_string()))
        .chain(a.fields.iter().cloned())
        .collect();
    let facts_map: Map<String, Value> = facts.iter().map(|(k, v)| (k.clone(), json!(v))).collect();
    match t.kind {
        AlertKind::Slack => {
            let fields: Vec<Value> = facts
                .iter()
                .map(|(k, v)| json!({ "type": "mrkdwn", "text": format!("*{k}*\n{v}") }))
                .collect();
            let mut blocks = vec![
                json!({ "type": "section", "text": { "type": "mrkdwn", "text": format!("*{}*\n{}", a.title, a.text) } }),
                json!({ "type": "section", "fields": fields }),
            ];
            if let Some(l) = link {
                blocks.push(json!({ "type": "actions", "elements": [
                    { "type": "button", "text": { "type": "plain_text", "text": "Open Tokenrein" }, "url": l }
                ]}));
            }
            json!({ "text": format!("{}: {}", a.title, a.text), "blocks": blocks })
        }
        AlertKind::Teams => {
            let facts: Vec<Value> = facts
                .iter()
                .map(|(k, v)| json!({ "title": k, "value": v }))
                .collect();
            let actions: Vec<Value> = link
                .map(|l| json!({ "type": "Action.OpenUrl", "title": "Open Tokenrein", "url": l }))
                .into_iter()
                .collect();
            json!({
                "type": "message",
                "attachments": [{
                    "contentType": "application/vnd.microsoft.card.adaptive",
                    "content": {
                        "$schema": "http://adaptivecards.io/schemas/adaptive-card.json",
                        "type": "AdaptiveCard",
                        "version": "1.4",
                        "body": [
                            { "type": "TextBlock", "text": a.title, "weight": "Bolder", "size": "Medium", "wrap": true },
                            { "type": "TextBlock", "text": a.text, "wrap": true },
                            { "type": "FactSet", "facts": facts }
                        ],
                        "actions": actions
                    }
                }]
            })
        }
        AlertKind::Pagerduty => {
            let mut details = facts_map;
            details.insert("detail".into(), json!(a.text));
            json!({
                "routing_key": t.url_or_key,
                "event_action": "trigger",
                "dedup_key": format!("tokenrein:{org}:{}", a.dedup),
                "payload": {
                    "summary": a.title,
                    "severity": a.severity.as_str(),
                    "source": "tokenrein",
                    "component": org,
                    "group": event_name(a.event),
                    "custom_details": details,
                },
                "links": link.map(|l| vec![json!({ "href": l, "text": "Open Tokenrein" })]).unwrap_or_default(),
            })
        }
        AlertKind::Webhook => json!({
            "event": event_name(a.event),
            "org": org,
            "severity": a.severity.as_str(),
            "title": a.title,
            "text": a.text,
            "fields": facts_map,
            "link": link,
            "sent_at": Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true),
        }),
    }
}

async fn deliver(
    http: &reqwest::Client,
    pagerduty: &str,
    org: &str,
    t: &Target,
    a: &Alert,
    link: Option<&str>,
) -> Result<(), String> {
    let body = serde_json::to_vec(&payload(t, org, a, link)).map_err(|e| e.to_string())?;
    let url = match t.kind {
        AlertKind::Pagerduty => pagerduty,
        _ => t.url_or_key.as_str(),
    };
    let mut last = String::new();
    for attempt in 0..ATTEMPTS {
        if attempt > 0 {
            tokio::time::sleep(Duration::from_millis(500 * 3u64.pow(attempt - 1))).await;
        }
        let mut req = http
            .post(url)
            .timeout(Duration::from_secs(10))
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .header(reqwest::header::USER_AGENT, "Tokenrein-Alerts/1");
        if let Some(secret) = &t.signing {
            let ts = Utc::now().timestamp().to_string();
            req = req
                .header("x-tokenrein-timestamp", &ts)
                .header("x-tokenrein-signature", sign(secret, &ts, &body));
        }
        match req.body(body.clone()).send().await {
            Ok(r) if r.status().is_success() => return Ok(()),
            Ok(r) if r.status().is_client_error() && r.status().as_u16() != 429 => {
                return Err(format!("HTTP {}", r.status().as_u16()));
            }
            Ok(r) => last = format!("HTTP {}", r.status().as_u16()),
            Err(e) if e.is_timeout() => last = "timed out".into(),
            Err(_) => last = "could not connect".into(),
        }
    }
    Err(last)
}

/// `sha256=<hex>` of HMAC-SHA256(secret, "<timestamp>.<body>"). Receivers recompute it and
/// reject old timestamps to stop replays.
pub fn sign(secret: &str, timestamp: &str, body: &[u8]) -> String {
    let mut mac =
        Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("HMAC accepts any key length");
    mac.update(timestamp.as_bytes());
    mac.update(b".");
    mac.update(body);
    format!("sha256={}", hex::encode(mac.finalize().into_bytes()))
}

/// Seal a new channel's URL (or routing key), and for webhooks a new signing secret. Returns the
/// channel and, for webhooks, the signing secret to show once.
pub fn new_channel(
    st: &AppState,
    org: &str,
    kind: AlertKind,
    name: &str,
    target: &str,
    events: Vec<AlertEvent>,
) -> anyhow::Result<(AlertChannel, Option<String>)> {
    let rt = st.rt();
    let secrets = rt.secrets().ok_or_else(|| {
        anyhow::anyhow!("set the secrets key on the proxy to save alert channels")
    })?;
    let id = format!("ch_{}", &uuid::Uuid::new_v4().simple().to_string()[..10]);
    let signing = (kind == AlertKind::Webhook).then(|| crate::auth::random_token("whsec_"));
    let chars: Vec<char> = target.chars().collect();
    let channel = AlertChannel {
        sealed: secrets.seal(&context(org, &id), target)?,
        last4: chars[chars.len().saturating_sub(4)..].iter().collect(),
        signing_sealed: match &signing {
            Some(s) => Some(secrets.seal(&signing_context(org, &id), s)?),
            None => None,
        },
        id,
        kind,
        name: name.to_string(),
        events,
        created_at: Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true),
    };
    Ok((channel, signing))
}

/// Send a test alert to one channel now (no quiet period). Returns the delivery error, if any.
pub async fn test(st: &AppState, org: &str, channel: &AlertChannel) -> Result<(), String> {
    let rt = st.rt();
    let secrets = rt.secrets().ok_or("the secrets key is not set")?;
    let t = Target {
        id: channel.id.clone(),
        kind: channel.kind,
        url_or_key: secrets
            .open(&context(org, &channel.id), &channel.sealed)
            .map_err(|_| "the saved URL cannot be decrypted with the current secrets key")?,
        signing: channel
            .signing_sealed
            .as_ref()
            .and_then(|s| secrets.open(&signing_context(org, &channel.id), s).ok()),
    };
    let alert = Alert {
        event: AlertEvent::BudgetWarning,
        severity: Severity::Info,
        title: "Tokenrein test alert".into(),
        text: format!("Alerts from org {org} reach this channel."),
        dedup: "test".into(),
        quiet_secs: 1,
        tab: None,
        fields: vec![],
    };
    let link = link(st, None);
    let result = deliver(
        &st.http,
        &pagerduty_url(st),
        org,
        &t,
        &alert,
        link.as_deref(),
    )
    .await;
    let mut conn = st.ledger.conn();
    record_status(&mut conn, org, &t.id, result.clone().err()).await;
    result
}

/// Last delivery result per channel (`at`, `ok`, `error`), in the channels' order.
pub async fn statuses(
    st: &AppState,
    org: &str,
    channels: &[AlertChannel],
) -> anyhow::Result<Vec<HashMap<String, String>>> {
    if channels.is_empty() {
        return Ok(Vec::new());
    }
    let mut pipe = redis::pipe();
    for c in channels {
        pipe.hgetall(Keys::new(org).alert_status(&c.id));
    }
    let mut conn = st.ledger.conn();
    Ok(pipe.query_async(&mut conn).await?)
}

/// An alert for a request refused because a budget is used up (from the activity stream).
pub fn blocked_alert(event: &Value) -> Option<(String, Alert)> {
    if event.get("decision").and_then(Value::as_str) != Some("blocked_budget") {
        return None;
    }
    let s = |k: &str| {
        event
            .get(k)
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string()
    };
    let scope = s("blocked_scope");
    let alert = Alert {
        event: AlertEvent::BudgetBlocked,
        severity: Severity::Critical,
        title: format!("Budget used up: {scope}"),
        text: format!(
            "Tokenrein refused a request from {}/{} (run {}) because the {scope} budget is used up.",
            s("project"),
            s("agent"),
            s("run_id")
        ),
        dedup: format!("blocked:{scope}"),
        quiet_secs: 1800,
        tab: Some("runs"),
        fields: vec![
            ("Budget".into(), scope),
            ("Project".into(), s("project")),
            ("Agent".into(), s("agent")),
        ],
    };
    Some((s("org"), alert))
}

/// Every minute, warn about budgets past 80% of their limit (once per budget period).
pub fn spawn_budget_watch(state: Arc<AppState>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(60));
        loop {
            tick.tick().await;
            if let Err(e) = watch_budgets(&state).await {
                tracing::warn!(error = %e, "budget warning check failed");
            }
        }
    })
}

/// One pass of the budget warning check.
pub async fn watch_budgets(st: &AppState) -> anyhow::Result<()> {
    let rt = st.rt();
    let now = Utc::now();
    let ym = now.format("%Y%m").to_string();
    let ymd = now.format("%Y%m%d").to_string();
    let day_start = now
        .date_naive()
        .and_hms_opt(0, 0, 0)
        .map(|t| t.and_utc().timestamp())
        .unwrap_or(0);
    for org in rt.cfg.orgs.iter().filter(|o| !o.alert_channels.is_empty()) {
        let keys = Keys::new(&org.id);
        // (budget key, label, quiet period)
        let mut budgets: Vec<(String, String, u64)> =
            vec![(keys.org_monthly(&ym), "org monthly".into(), 32 * 86_400)];
        for p in &org.projects {
            budgets.push((
                keys.project_monthly(&p.id, &ym),
                format!("project {} monthly", p.id),
                32 * 86_400,
            ));
        }
        for (member, last_seen) in st.ledger.recent_agents(&org.id, 500).await? {
            if last_seen < day_start {
                continue;
            }
            if let Some((project, agent)) = member.split_once('/') {
                budgets.push((
                    keys.agent_daily(project, agent, &ymd),
                    format!("agent {member} daily"),
                    26 * 3600,
                ));
            }
        }
        let keys_only: Vec<String> = budgets.iter().map(|b| b.0.clone()).collect();
        let values = st.ledger.budgets(&keys_only).await?;
        for ((key, label, quiet), b) in budgets.into_iter().zip(values) {
            let num = |f: &str| b.get(f).and_then(|v| v.parse::<f64>().ok());
            let (Some(limit), Some(spent)) = (num("limit"), num("spent")) else {
                continue;
            };
            let mode = b.get("mode").map(String::as_str).unwrap_or("skip");
            if limit <= 0.0 || mode == "skip" || spent < limit * WARN_AT {
                continue;
            }
            let pct = (spent / limit * 100.0).floor();
            let alert = Alert {
                event: AlertEvent::BudgetWarning,
                severity: Severity::Warning,
                title: format!("The {label} budget is {pct}% used"),
                text: format!(
                    "${:.2} of ${:.2} spent ({mode} mode).",
                    spent / 1e6,
                    limit / 1e6
                ),
                dedup: format!("warn:{key}"),
                quiet_secs: quiet,
                tab: Some("spend"),
                fields: vec![("Budget".into(), label)],
            };
            notify(st, &org.id, alert).await;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signatures_cover_timestamp_and_body() {
        let a = sign("whsec_x", "1700000000", b"{}");
        assert!(a.starts_with("sha256=") && a.len() == 7 + 64);
        assert_ne!(a, sign("whsec_x", "1700000001", b"{}"));
        assert_ne!(a, sign("whsec_y", "1700000000", b"{}"));
        assert_eq!(a, sign("whsec_x", "1700000000", b"{}"));
    }

    #[test]
    fn only_budget_refusals_alert() {
        let ev = json!({"decision": "blocked_budget", "org": "acme", "blocked_scope": "run:r1",
                        "project": "bot", "agent": "a", "run_id": "r1"});
        let (org, a) = blocked_alert(&ev).unwrap();
        assert_eq!(org, "acme");
        assert_eq!(a.dedup, "blocked:run:r1");
        assert!(blocked_alert(&json!({"decision": "allowed"})).is_none());
    }
}
