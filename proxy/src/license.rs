//! License keys and the free tier.
//!
//! Free installs get every guardrail for up to [`FREE_DEVELOPERS`] developers and
//! [`FREE_SPEND_MICROS`] of AI spend a month. Going over only shows reminders: agents are never
//! blocked. A license key unlocks the paid features ([`Feature`]): Team for SSO, audit export,
//! usage history past [`FREE_HISTORY_DAYS`] days, PagerDuty and the Cursor import; Enterprise also
//! for more than one org.
//!
//! A key is a JWT signed with Ed25519 by the vendor and checked offline against the public key
//! built into the binary. It comes from the dashboard (kept in the managed config) or from
//! `TOKENREIN_LICENSE` / `TOKENREIN_LICENSE_FILE`. For [`GRACE_DAYS`] after it expires everything
//! keeps working; after that the install is back on the free tier, with its data and settings
//! kept.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use chrono::{DateTime, Utc};
use jsonwebtoken::{Algorithm, DecodingKey, Validation};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::admin::{edit_error, err, unavailable};
use crate::auth::Admin;
use crate::proxy::AppState;

pub const FREE_DEVELOPERS: u64 = 25;
pub const FREE_SPEND_MICROS: i64 = 10_000 * 1_000_000;
/// Usage history (spend reports and exports) the free tier can look back over.
pub const FREE_HISTORY_DAYS: u32 = 90;
/// Days a key keeps working after it expires.
pub const GRACE_DAYS: i64 = 30;
/// Days before expiry when reminders start.
const EXPIRING_DAYS: i64 = 30;
/// Developers are the distinct people (the `user` on runs) seen in this many days.
const DEVELOPER_WINDOW_DAYS: i64 = 30;
const USAGE_CACHE: Duration = Duration::from_secs(600);
pub const PRICING_URL: &str = "https://tokenrein.com/#pricing";
const DAY: i64 = 86_400;

/// The vendor's public key; keys are signed with the matching private key, which is not in the
/// repository.
pub const PUBLIC_KEY: &str = include_str!("license_pub.pem");

/// The public key keys are checked against: the built-in one, or in debug builds (the test
/// suites) one given in `TOKENREIN_LICENSE_TEST_PUBLIC_KEY`.
pub fn public_key(env: &dyn Fn(&str) -> Option<String>) -> String {
    #[cfg(debug_assertions)]
    if let Some(k) = env("TOKENREIN_LICENSE_TEST_PUBLIC_KEY") {
        return k;
    }
    let _ = env;
    PUBLIC_KEY.to_string()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Plan {
    Team,
    Enterprise,
}

impl Plan {
    pub fn includes(self, f: Feature) -> bool {
        self == Plan::Enterprise || f.plan() == Plan::Team
    }

    fn name(self) -> &'static str {
        match self {
            Plan::Team => "Team",
            Plan::Enterprise => "Enterprise",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Feature {
    Sso,
    AuditExport,
    LongHistory,
    Pagerduty,
    CursorImport,
    MultipleOrgs,
}

impl Feature {
    pub const ALL: [Feature; 6] = [
        Feature::Sso,
        Feature::AuditExport,
        Feature::LongHistory,
        Feature::Pagerduty,
        Feature::CursorImport,
        Feature::MultipleOrgs,
    ];

    /// The cheapest plan that includes the feature.
    pub fn plan(self) -> Plan {
        match self {
            Feature::MultipleOrgs => Plan::Enterprise,
            _ => Plan::Team,
        }
    }

    fn id(self) -> &'static str {
        match self {
            Feature::Sso => "sso",
            Feature::AuditExport => "audit_export",
            Feature::LongHistory => "long_history",
            Feature::Pagerduty => "pagerduty",
            Feature::CursorImport => "cursor_import",
            Feature::MultipleOrgs => "multiple_orgs",
        }
    }

    fn describe(self) -> &'static str {
        match self {
            Feature::Sso => "Single sign-on",
            Feature::AuditExport => "Audit log export",
            Feature::LongHistory => "Usage history past 90 days",
            Feature::Pagerduty => "PagerDuty alerts",
            Feature::CursorImport => "The Cursor usage import",
            Feature::MultipleOrgs => "More than one org",
        }
    }
}

/// What a license key says.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Claims {
    pub iss: String,
    /// The customer.
    pub sub: String,
    pub plan: Plan,
    /// Licensed developers; none means no limit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub developers: Option<u64>,
    pub iat: i64,
    pub exp: i64,
    pub jti: String,
}

/// Where the install stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Standing {
    Free,
    Licensed,
    /// Within 30 days of expiry.
    Expiring,
    /// Expired, but paid features keep working for [`GRACE_DAYS`].
    Grace,
    /// Back on the free tier.
    Expired,
}

/// The key the install runs with, checked once when the config loads.
#[derive(Debug, Clone, Default)]
pub enum Key {
    #[default]
    None,
    Valid {
        claims: Claims,
        source: &'static str,
    },
    /// A key was given but does not verify; the install runs as free.
    Invalid {
        reason: String,
        source: &'static str,
    },
}

impl Key {
    /// The dashboard's key wins over the environment's.
    pub fn load(managed: Option<&str>, env: Option<&str>, public_pem: &str) -> Self {
        let (token, source) = match (managed, env) {
            (Some(t), _) if !t.trim().is_empty() => (t, "dashboard"),
            (_, Some(t)) if !t.trim().is_empty() => (t, "environment"),
            _ => return Key::None,
        };
        match verify(token, public_pem) {
            Ok(claims) => Key::Valid { claims, source },
            Err(e) => {
                tracing::error!(source, error = %format!("{e:#}"), "license key is not valid; running as free");
                Key::Invalid {
                    reason: format!("{e:#}"),
                    source,
                }
            }
        }
    }

    pub fn claims(&self) -> Option<&Claims> {
        match self {
            Key::Valid { claims, .. } => Some(claims),
            _ => None,
        }
    }

    pub fn standing(&self, now: DateTime<Utc>) -> Standing {
        let Some(c) = self.claims() else {
            return Standing::Free;
        };
        let now = now.timestamp();
        if now <= c.exp - EXPIRING_DAYS * DAY {
            Standing::Licensed
        } else if now <= c.exp {
            Standing::Expiring
        } else if now <= c.exp + GRACE_DAYS * DAY {
            Standing::Grace
        } else {
            Standing::Expired
        }
    }

    /// The key's claims while its paid features work (including the grace period).
    fn active(&self, now: DateTime<Utc>) -> Option<&Claims> {
        match self.standing(now) {
            Standing::Licensed | Standing::Expiring | Standing::Grace => self.claims(),
            Standing::Free | Standing::Expired => None,
        }
    }

    pub fn allows(&self, f: Feature, now: DateTime<Utc>) -> bool {
        self.active(now).is_some_and(|c| c.plan.includes(f))
    }
}

/// Checks a key's signature (Ed25519 only) and issuer. Expiry is handled by [`Key::standing`].
pub fn verify(token: &str, public_pem: &str) -> Result<Claims> {
    let key = DecodingKey::from_ed_pem(public_pem.as_bytes()).context("license public key")?;
    let mut v = Validation::new(Algorithm::EdDSA);
    v.set_issuer(&["tokenrein"]);
    v.set_required_spec_claims(&["exp", "iss", "sub"]);
    v.validate_exp = false;
    let data = jsonwebtoken::decode::<Claims>(token.trim(), &key, &v)
        .context("not a valid Tokenrein license key")?;
    Ok(data.claims)
}

/// Developers and spend, measured from the ledger across all orgs.
#[derive(Debug, Clone, Copy, Default, Serialize)]
pub struct Usage {
    pub developers: u64,
    /// This calendar month, micro-USD.
    pub spend_micros: i64,
}

impl Usage {
    /// Over the free tier, or over the developers a key allows.
    pub fn over_limit(&self, key: &Key, now: DateTime<Utc>) -> bool {
        match key.active(now) {
            Some(c) => c.developers.is_some_and(|d| self.developers > d),
            None => self.developers > FREE_DEVELOPERS || self.spend_micros > FREE_SPEND_MICROS,
        }
    }
}

/// Usage, measured at most every ten minutes.
#[derive(Default)]
pub struct UsageCache(std::sync::Mutex<Option<(Instant, Usage)>>);

pub async fn usage(st: &AppState) -> Result<Usage> {
    if let Some((at, u)) = *st.license_usage.0.lock().unwrap_or_else(|e| e.into_inner())
        && at.elapsed() < USAGE_CACHE
    {
        return Ok(u);
    }
    let u = measure(st, Utc::now()).await?;
    *st.license_usage.0.lock().unwrap_or_else(|e| e.into_inner()) = Some((Instant::now(), u));
    crate::metrics::license_usage(&u, u.over_limit(&st.rt().license, Utc::now()));
    Ok(u)
}

/// Distinct people on runs in the last 30 days, and this month's spend, from the ledger's daily
/// spend breakdown.
pub async fn measure(st: &AppState, now: DateTime<Utc>) -> Result<Usage> {
    let month = now.format("%Y%m").to_string();
    let days: Vec<String> = (0..DEVELOPER_WINDOW_DAYS.max(31))
        .map(|i| {
            (now - chrono::Duration::days(i))
                .format("%Y%m%d")
                .to_string()
        })
        .collect();
    let mut developers = HashSet::new();
    let mut spend_micros = 0;
    for org in &st.rt().cfg.orgs {
        let maps = st.ledger.spend_days(&org.id, &days).await?;
        for (i, (day, m)) in days.iter().zip(maps).enumerate() {
            for (field, v) in m {
                if let Some(user) = field
                    .strip_prefix("user:")
                    .and_then(|f| f.strip_suffix(":n"))
                {
                    if user != "-" && (i as i64) < DEVELOPER_WINDOW_DAYS {
                        developers.insert(user.to_string());
                    }
                } else if day.starts_with(&month)
                    && field.starts_with("project:")
                    && field.ends_with(":c")
                {
                    spend_micros += v;
                }
            }
        }
    }
    Ok(Usage {
        developers: developers.len() as u64,
        spend_micros,
    })
}

/// `Err` is the 403 to return when the install's key does not include `f`.
#[allow(clippy::result_large_err)]
pub fn require(st: &AppState, f: Feature) -> Result<(), Response> {
    if allows(st, f) {
        return Ok(());
    }
    let plan = f.plan();
    Err((
        StatusCode::FORBIDDEN,
        Json(json!({
            "error": format!("{} needs a Tokenrein {} license: {PRICING_URL}", f.describe(), plan.name()),
            "code": "license_required",
            "feature": f,
            "plan": plan,
        })),
    )
        .into_response())
}

/// Whether the install's key includes `f`, for code that skips rather than refuses.
pub fn allows(st: &AppState, f: Feature) -> bool {
    st.rt().license.allows(f, Utc::now())
}

pub fn routes() -> Router<Arc<AppState>> {
    Router::new().route(
        "/admin/license",
        get(get_license).put(put_license).delete(delete_license),
    )
}

/// The license, usage against its limits, and the features it unlocks.
pub async fn status(st: &AppState) -> Result<Value> {
    let rt = st.rt();
    let now = Utc::now();
    let usage = usage(st).await?;
    let key = &rt.license;
    let (source, invalid) = match key {
        Key::None => (None, None),
        Key::Valid { source, .. } => (Some(*source), None),
        Key::Invalid { reason, source } => (Some(*source), Some(reason.clone())),
    };
    let features: serde_json::Map<String, Value> = Feature::ALL
        .iter()
        .map(|f| {
            (
                f.id().to_string(),
                json!({ "allowed": key.allows(*f, now), "plan": f.plan() }),
            )
        })
        .collect();
    let c = key.claims();
    let at = |t: i64| DateTime::<Utc>::from_timestamp(t, 0);
    Ok(json!({
        "standing": key.standing(now),
        "plan": c.map(|c| c.plan),
        "customer": c.map(|c| &c.sub),
        "expires_at": c.and_then(|c| at(c.exp)),
        "grace_ends_at": c.and_then(|c| at(c.exp + GRACE_DAYS * DAY)),
        "source": source,
        "invalid": invalid,
        "usage": usage,
        "limits": match key.active(now) {
            Some(c) => json!({ "developers": c.developers, "spend_micros": null }),
            None => json!({ "developers": FREE_DEVELOPERS, "spend_micros": FREE_SPEND_MICROS }),
        },
        "over_limit": usage.over_limit(key, now),
        "orgs": rt.cfg.orgs.len(),
        "features": features,
        "pricing_url": PRICING_URL,
    }))
}

/// Any signed-in person may read it: it drives the dashboard's reminders.
async fn get_license(State(st): State<Arc<AppState>>, _admin: Admin) -> Response {
    match status(&st).await {
        Ok(v) => Json(v).into_response(),
        Err(e) => unavailable(e),
    }
}

#[derive(Deserialize)]
struct LicenseBody {
    key: String,
}

async fn put_license(
    State(st): State<Arc<AppState>>,
    admin: Admin,
    Json(body): Json<LicenseBody>,
) -> Response {
    if let Err(r) = admin.instance() {
        return r;
    }
    let key = body.key.trim().to_string();
    let claims = match verify(&key, &st.rt().license_public_key()) {
        Ok(c) => c,
        Err(e) => return err(StatusCode::BAD_REQUEST, &format!("{e:#}")),
    };
    let target = format!("{} ({})", claims.sub, claims.plan.name());
    let change = admin.change("license.update", None, &target);
    if let Err(e) = crate::control::edit_license(&st, change, Some(key)).await {
        return edit_error(e);
    }
    match status(&st).await {
        Ok(v) => Json(v).into_response(),
        Err(e) => unavailable(e),
    }
}

async fn delete_license(State(st): State<Arc<AppState>>, admin: Admin) -> Response {
    if let Err(r) = admin.instance() {
        return r;
    }
    let change = admin.change("license.remove", None, "license");
    match crate::control::edit_license(&st, change, None).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => edit_error(e),
    }
}

/// Once a day: log when the install is over its limits or its key is running out; the alert
/// channels hear about it once a week.
pub fn spawn_reminders(st: Arc<AppState>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(DAY as u64));
        loop {
            tick.tick().await;
            if let Err(e) = remind(&st).await {
                tracing::warn!(error = %format!("{e:#}"), "license check failed");
            }
        }
    })
}

/// The reminder for the install's current usage and key, if one is due.
pub fn reminder(usage: &Usage, key: &Key, now: DateTime<Utc>) -> Option<String> {
    let standing = key.standing(now);
    if usage.over_limit(key, now) {
        return Some(if key.active(now).is_some() {
            format!(
                "Tokenrein has seen {} developers in the last 30 days, more than its license covers. Agents keep working. See {PRICING_URL}",
                usage.developers
            )
        } else {
            format!(
                "Tokenrein has seen {} developers in the last 30 days and ${:.0} of AI spend this month; the free tier covers {FREE_DEVELOPERS} developers and ${}. Agents keep working. See {PRICING_URL}",
                usage.developers,
                usage.spend_micros as f64 / 1e6,
                FREE_SPEND_MICROS / 1_000_000
            )
        });
    }
    matches!(standing, Standing::Expiring | Standing::Grace).then(|| {
        format!(
            "The Tokenrein license key expires soon; paid features stop {GRACE_DAYS} days after it expires. Write to hello@tokenrein.com to renew."
        )
    })
}

async fn remind(st: &AppState) -> Result<()> {
    let usage = usage(st).await?;
    let rt = st.rt();
    if let Some(text) = reminder(&usage, &rt.license, Utc::now()) {
        tracing::warn!(developers = usage.developers, "{text}");
        crate::alerts::license_reminder(st, &text).await;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;
    use jsonwebtoken::{EncodingKey, Header};

    // A throwaway keypair for tests only; real keys are signed with the vendor's key.
    const TEST_PRIVATE: &str = "-----BEGIN PRIVATE KEY-----\nMC4CAQAwBQYDK2VwBCIEIF66SpbBJj+nPj6c43RYYkKnUzjgJmLDk4PGGA4RYtNR\n-----END PRIVATE KEY-----\n";
    const TEST_PUBLIC: &str = "-----BEGIN PUBLIC KEY-----\nMCowBQYDK2VwAyEAMQihtaQAdJ+4uiI38NnRbraKnTV6kF1WWmAtCatLNdo=\n-----END PUBLIC KEY-----\n";
    const B64: base64::engine::GeneralPurpose = base64::engine::general_purpose::URL_SAFE_NO_PAD;

    fn claims(plan: Plan, exp: i64) -> Claims {
        Claims {
            iss: "tokenrein".into(),
            sub: "Acme".into(),
            plan,
            developers: Some(50),
            iat: 0,
            exp,
            jti: "lic_1".into(),
        }
    }

    fn sign(c: &Claims) -> String {
        let key = EncodingKey::from_ed_pem(TEST_PRIVATE.as_bytes()).unwrap();
        jsonwebtoken::encode(&Header::new(Algorithm::EdDSA), c, &key).unwrap()
    }

    fn valid(c: Claims) -> Key {
        Key::Valid {
            claims: c,
            source: "test",
        }
    }

    #[test]
    fn a_signed_key_verifies() {
        let c = claims(Plan::Team, 2_000_000_000);
        assert_eq!(verify(&sign(&c), TEST_PUBLIC).unwrap(), c);
    }

    #[test]
    fn a_changed_key_or_another_signer_is_refused() {
        let token = sign(&claims(Plan::Team, 2_000_000_000));
        let mut parts: Vec<String> = token.split('.').map(str::to_string).collect();
        let mut body: Value = serde_json::from_slice(&B64.decode(&parts[1]).unwrap()).unwrap();
        body["plan"] = json!("enterprise");
        parts[1] = B64.encode(body.to_string());
        assert!(
            verify(&parts.join("."), TEST_PUBLIC).is_err(),
            "tampered claims"
        );
        assert!(verify(&token, PUBLIC_KEY).is_err(), "signed by another key");
    }

    #[test]
    fn unsigned_hmac_and_foreign_keys_are_refused() {
        let c = claims(Plan::Enterprise, 2_000_000_000);
        let hmac = jsonwebtoken::encode(
            &Header::new(Algorithm::HS256),
            &c,
            &EncodingKey::from_secret(TEST_PUBLIC.as_bytes()),
        )
        .unwrap();
        assert!(verify(&hmac, TEST_PUBLIC).is_err());
        let none = format!(
            "{}.{}.",
            B64.encode(r#"{"alg":"none","typ":"JWT"}"#),
            B64.encode(serde_json::to_string(&c).unwrap())
        );
        assert!(verify(&none, TEST_PUBLIC).is_err());
        let mut other = c.clone();
        other.iss = "someone".into();
        assert!(verify(&sign(&other), TEST_PUBLIC).is_err());
    }

    #[test]
    fn a_key_expires_after_a_grace_period() {
        let exp = DateTime::parse_from_rfc3339("2027-01-31T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let key = valid(claims(Plan::Team, exp.timestamp()));
        let at = |days: i64| exp + chrono::Duration::days(days);
        assert_eq!(key.standing(at(-60)), Standing::Licensed);
        assert_eq!(key.standing(at(-10)), Standing::Expiring);
        assert_eq!(key.standing(at(10)), Standing::Grace);
        assert!(key.allows(Feature::Sso, at(10)));
        assert_eq!(key.standing(at(31)), Standing::Expired);
        assert!(!key.allows(Feature::Sso, at(31)));
        assert_eq!(Key::None.standing(at(0)), Standing::Free);
    }

    #[test]
    fn team_covers_everything_but_multiple_orgs() {
        let now = Utc::now();
        let team = valid(claims(Plan::Team, now.timestamp() + DAY * 100));
        let ent = valid(claims(Plan::Enterprise, now.timestamp() + DAY * 100));
        for f in Feature::ALL {
            assert!(ent.allows(f, now), "{f:?}");
            assert_eq!(team.allows(f, now), f != Feature::MultipleOrgs, "{f:?}");
            assert!(!Key::None.allows(f, now));
        }
    }

    #[test]
    fn limits_are_the_free_tier_or_the_licensed_seats() {
        let now = Utc::now();
        let at = |developers, spend_usd: i64| Usage {
            developers,
            spend_micros: spend_usd * 1_000_000,
        };
        assert!(!at(25, 10_000).over_limit(&Key::None, now));
        assert!(at(26, 0).over_limit(&Key::None, now));
        assert!(at(1, 10_001).over_limit(&Key::None, now));
        let team = valid(claims(Plan::Team, now.timestamp() + DAY * 100));
        assert!(!at(50, 1_000_000).over_limit(&team, now));
        assert!(at(51, 0).over_limit(&team, now));
        let lapsed = valid(claims(Plan::Team, now.timestamp() - DAY * 40));
        assert!(
            at(26, 0).over_limit(&lapsed, now),
            "an expired key is the free tier"
        );
    }

    #[test]
    fn reminders_only_when_over_or_running_out() {
        let now = Utc::now();
        let quiet = Usage::default();
        let busy = Usage {
            developers: 40,
            spend_micros: 0,
        };
        assert_eq!(reminder(&quiet, &Key::None, now), None);
        assert!(
            reminder(&busy, &Key::None, now)
                .unwrap()
                .contains("free tier")
        );
        let soon = valid(claims(Plan::Team, now.timestamp() + DAY * 5));
        assert!(
            reminder(&quiet, &soon, now)
                .unwrap()
                .contains("expires soon")
        );
    }

    #[test]
    fn a_bad_key_runs_as_free() {
        assert!(matches!(
            Key::load(Some("not-a-key"), None, TEST_PUBLIC),
            Key::Invalid { .. }
        ));
        assert!(matches!(
            Key::load(None, Some("  "), TEST_PUBLIC),
            Key::None
        ));
        let c = claims(Plan::Team, 2_000_000_000);
        let k = Key::load(Some(&sign(&c)), Some("not-a-key"), TEST_PUBLIC);
        assert!(
            matches!(
                k,
                Key::Valid {
                    source: "dashboard",
                    ..
                }
            ),
            "the dashboard key wins"
        );
    }
}
