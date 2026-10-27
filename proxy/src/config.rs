use std::collections::{HashMap, HashSet};

use anyhow::{Context, Result, bail};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::providers::Provider;
use crate::secrets::SecretBox;

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    Skip,
    #[default]
    Check,
    Enforce,
    /// Like enforce, but the run is paused with an approval request instead of failing: an
    /// approver can give it more budget or stop it. Run budgets only.
    Ask,
}

impl Mode {
    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Skip => "skip",
            Mode::Check => "check",
            Mode::Enforce => "enforce",
            Mode::Ask => "ask",
        }
    }
}

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum FailMode {
    #[default]
    Open,
    Closed,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BudgetPolicy {
    pub limit_usd: f64,
    #[serde(default)]
    pub mode: Mode,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Budgets {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub org_monthly: Option<BudgetPolicy>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project_monthly: Option<BudgetPolicy>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_daily: Option<BudgetPolicy>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run: Option<BudgetPolicy>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_run_usd: Option<f64>,
    /// Runaway-loop detection for runs (see `crate::loops`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub loops: Option<LoopPolicy>,
}

/// What counts as a runaway run, and what happens to it.
#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct LoopPolicy {
    #[serde(default)]
    pub action: LoopAction,
    /// The same request (or tool call) this many times within `repeat_window_secs`.
    #[serde(default = "default_max_repeats")]
    pub max_repeats: u32,
    #[serde(default = "default_repeat_window")]
    pub repeat_window_secs: u32,
    /// Spend within one minute, in US dollars.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_usd_per_minute: Option<f64>,
    /// Model calls in one run, including its sub-agent runs' own counts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_calls_per_run: Option<u32>,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum LoopAction {
    /// Alert only.
    Warn,
    /// Pause the run and ask an approver whether it may continue.
    #[default]
    Pause,
    /// Stop the run for good.
    Kill,
}

fn default_max_repeats() -> u32 {
    6
}
fn default_repeat_window() -> u32 {
    600
}

impl Budgets {
    /// Project settings override the org's; `org_monthly` is only read from the org.
    pub fn effective(org: &Budgets, project: &Budgets) -> Budgets {
        Budgets {
            org_monthly: org.org_monthly,
            project_monthly: project.project_monthly.or(org.project_monthly),
            agent_daily: project.agent_daily.or(org.agent_daily),
            run: project.run.or(org.run),
            max_run_usd: project.max_run_usd.or(org.max_run_usd),
            loops: project.loops.or(org.loops),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    pub listen: String,
    #[serde(default = "default_admin_listen")]
    pub admin_listen: String,
    #[serde(default = "default_admin_token_env")]
    pub admin_token_env: String,
    /// Env var holding the 32-byte hex key that encrypts provider API keys saved from the dashboard.
    /// During a key rotation, `<this>_PREVIOUS` holds the old key: saved keys are still opened
    /// with it until `tokenrein rotate-secrets-key` re-encrypts them.
    #[serde(default = "default_secrets_key_env")]
    pub secrets_key_env: String,
    #[serde(default = "default_upstream_timeout")]
    pub upstream_timeout_secs: u64,
    #[serde(default = "default_max_output")]
    pub default_max_output_tokens: u64,
    #[serde(default = "default_max_body")]
    pub max_body_bytes: usize,
    /// Base URL of Cursor's Admin API, for the optional Cursor usage import.
    #[serde(default = "default_cursor_api_url")]
    pub cursor_api_url: String,
}

fn default_admin_listen() -> String {
    "127.0.0.1:8788".into()
}
fn default_admin_token_env() -> String {
    "TOKENREIN_ADMIN_TOKEN".into()
}
fn default_secrets_key_env() -> String {
    "TOKENREIN_SECRETS_KEY".into()
}
fn default_upstream_timeout() -> u64 {
    600
}
fn default_max_output() -> u64 {
    4096
}
fn default_max_body() -> usize {
    10 * 1024 * 1024
}
fn default_cursor_api_url() -> String {
    "https://api.cursor.com".into()
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RedisConfig {
    pub url: String,
    /// Each Redis call fails after this long; the org's fail mode then decides.
    #[serde(default = "default_redis_timeout_ms")]
    pub timeout_ms: u64,
}

fn default_redis_timeout_ms() -> u64 {
    1000
}

/// Postgres keeps every config version and the audit log. Optional: without it they live in
/// Redis (current config only, last 10,000 audit entries).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DatabaseConfig {
    /// Env var holding the connection URL (`postgres://user:password@host/db`).
    #[serde(default = "default_database_url_env")]
    pub url_env: String,
    #[serde(default = "default_database_max_connections")]
    pub max_connections: u32,
}

impl Default for DatabaseConfig {
    fn default() -> Self {
        Self {
            url_env: default_database_url_env(),
            max_connections: default_database_max_connections(),
        }
    }
}

/// Signing in to the dashboard and admin API (needs a database).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuthConfig {
    /// The dashboard's address as people reach it (`https://tokenrein.example.com`). Needed for
    /// single sign-on (the identity provider sends people back here) and full setup links;
    /// `https://` also marks session cookies Secure.
    #[serde(default)]
    pub public_url: Option<String>,
    /// Sessions end this long after sign-in...
    #[serde(default = "default_session_hours")]
    pub session_hours: u32,
    /// ...or after this long without activity.
    #[serde(default = "default_idle_minutes")]
    pub idle_minutes: u32,
    /// Allow email + password sign-in (turn off to require single sign-on).
    #[serde(default = "default_true")]
    pub password_login: bool,
}

impl Default for AuthConfig {
    fn default() -> Self {
        Self {
            public_url: None,
            session_hours: default_session_hours(),
            idle_minutes: default_idle_minutes(),
            password_login: true,
        }
    }
}

fn default_session_hours() -> u32 {
    12
}
fn default_idle_minutes() -> u32 {
    120
}
fn default_true() -> bool {
    true
}

fn default_database_url_env() -> String {
    "TOKENREIN_DATABASE_URL".into()
}
fn default_database_max_connections() -> u32 {
    10
}

/// Usage history for spend reports beyond Redis's 40 days: Parquet files in `dir`, read with
/// DuckDB. Off unless `dir` is set (or `TOKENREIN_HISTORY_DIR`).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HistoryConfig {
    /// Folder for the files; replicas may share it.
    #[serde(default)]
    pub dir: Option<String>,
    /// Days kept (default 400, about 13 months).
    #[serde(default = "default_history_retention_days")]
    pub retention_days: u32,
    /// Seconds between writes (default 30); events are held in memory until then.
    #[serde(default = "default_history_flush_secs")]
    pub flush_secs: u64,
}

impl Default for HistoryConfig {
    fn default() -> Self {
        Self {
            dir: None,
            retention_days: default_history_retention_days(),
            flush_secs: default_history_flush_secs(),
        }
    }
}

fn default_history_retention_days() -> u32 {
    400
}

fn default_history_flush_secs() -> u64 {
    30
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderConfig {
    pub base_url: String,
    pub api_key_env: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProvidersConfig {
    pub openai: Option<ProviderConfig>,
    pub anthropic: Option<ProviderConfig>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PriceOverride {
    pub provider: String,
    pub model: String,
    pub input_per_mtok: f64,
    pub output_per_mtok: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cached_input_per_mtok: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write_per_mtok: Option<f64>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectConfig {
    pub id: String,
    /// Bare key hashes, as written in the config file.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub virtual_key_hashes: Vec<String>,
    /// Keys created from the admin API / dashboard, with a name and a recognisable prefix.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub keys: Vec<VirtualKey>,
    #[serde(default)]
    pub budgets: Budgets,
    /// The guardrail pack last applied to this project, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pack: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct VirtualKey {
    /// sha256 of the key; the key itself is never stored.
    pub hash: String,
    pub name: String,
    /// The first characters of the key, shown so people can tell keys apart.
    pub prefix: String,
    pub created_at: String,
}

impl ProjectConfig {
    /// Every key hash that authenticates as this project.
    pub fn key_hashes(&self) -> impl Iterator<Item = &str> {
        self.virtual_key_hashes
            .iter()
            .map(String::as_str)
            .chain(self.keys.iter().map(|k| k.hash.as_str()))
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct OrgConfig {
    pub id: String,
    #[serde(default)]
    pub fail_mode: FailMode,
    #[serde(default)]
    pub budgets: Budgets,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub provider_key_env: HashMap<String, String>,
    /// Provider API keys saved from the dashboard, encrypted; they take precedence over the
    /// environment. Keyed by provider name.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub provider_secrets: HashMap<String, ProviderSecret>,
    /// Sent as `anthropic-workspace-id` on this org's Anthropic calls (unless the client sends
    /// one). Required by Anthropic keys that are not scoped to a workspace.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub anthropic_workspace_id: Option<String>,
    /// Usage imported from the Cursor team's Admin API, if set up.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor_import: Option<CursorImport>,
    /// Where alerts go (Slack, Microsoft Teams, webhooks, PagerDuty).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub alert_channels: Vec<AlertChannel>,
    #[serde(default)]
    pub projects: Vec<ProjectConfig>,
}

/// One place alerts are sent. The URL or routing key is a secret: it is stored encrypted and
/// only its last 4 characters are shown.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AlertChannel {
    pub id: String,
    pub kind: AlertKind,
    pub name: String,
    /// See [`crate::secrets::SecretBox::seal`].
    pub sealed: String,
    pub last4: String,
    /// Webhooks only: the HMAC-SHA256 signing secret, encrypted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signing_sealed: Option<String>,
    /// Events sent here; empty means all.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub events: Vec<AlertEvent>,
    pub created_at: String,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum AlertKind {
    /// Slack incoming webhook URL.
    Slack,
    /// Microsoft Teams incoming webhook (or Workflows) URL.
    Teams,
    /// Any HTTPS endpoint; requests are signed with HMAC-SHA256.
    Webhook,
    /// PagerDuty Events API v2 routing key.
    Pagerduty,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum AlertEvent {
    /// A budget passed 80% of its limit.
    BudgetWarning,
    /// A request was refused because a budget is used up.
    BudgetBlocked,
    /// A run is waiting for an approver.
    ApprovalRequested,
    /// A run looks like a runaway loop.
    LoopDetected,
}

/// Imports a Cursor team's usage events (see `crate::connectors`).
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CursorImport {
    /// The project the imported spend counts against.
    pub project: String,
    /// The team's Admin API key, encrypted like provider keys.
    pub key: ProviderSecret,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderSecret {
    /// See [`crate::secrets::SecretBox::seal`].
    pub sealed: String,
    /// The key's last four characters, to tell keys apart.
    pub last4: String,
    pub updated_at: String,
}

/// Where an org's key for one provider comes from, for the dashboard. Never holds the key.
#[derive(Debug, Clone, Serialize)]
pub struct ProviderKeyStatus {
    /// The provider has a `[providers.*]` section in the config file.
    pub configured: bool,
    /// "dashboard", "environment", or None when there is no key.
    pub source: Option<&'static str>,
    pub env_var: Option<String>,
    pub last4: Option<String>,
    pub updated_at: Option<String>,
    /// Why a saved key cannot be used (e.g. the secrets key is missing or changed).
    pub error: Option<String>,
}

/// Provider API keys go into HTTP headers: printable ASCII, no spaces.
pub fn valid_provider_key(s: &str) -> bool {
    (8..=512).contains(&s.len()) && s.bytes().all(|b| (0x21..=0x7e).contains(&b))
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub server: ServerConfig,
    pub redis: RedisConfig,
    #[serde(default)]
    pub database: DatabaseConfig,
    #[serde(default)]
    pub history: HistoryConfig,
    #[serde(default)]
    pub auth: AuthConfig,
    #[serde(default)]
    pub providers: ProvidersConfig,
    #[serde(default)]
    pub pricing: Vec<PriceOverride>,
    #[serde(default)]
    pub orgs: Vec<OrgConfig>,
}

/// Settings a container can set from the environment instead of editing the config file.
pub const ENV_OVERRIDES: &[(&str, &str)] = &[
    ("TOKENREIN_LISTEN", "server.listen"),
    ("TOKENREIN_ADMIN_LISTEN", "server.admin_listen"),
    ("TOKENREIN_REDIS_URL", "redis.url"),
    ("TOKENREIN_PUBLIC_URL", "auth.public_url"),
    ("TOKENREIN_HISTORY_DIR", "history.dir"),
    ("TOKENREIN_HISTORY_RETENTION_DAYS", "history.retention_days"),
    (
        "TOKENREIN_DATABASE_MAX_CONNECTIONS",
        "database.max_connections",
    ),
];

impl Config {
    /// Applies [`ENV_OVERRIDES`]; unset or empty variables leave the file's value.
    fn apply_env_overrides(&mut self, env: &dyn Fn(&str) -> Option<String>) {
        let get = |name: &str| env(name).filter(|v| !v.trim().is_empty());
        if let Some(v) = get("TOKENREIN_LISTEN") {
            self.server.listen = v;
        }
        if let Some(v) = get("TOKENREIN_ADMIN_LISTEN") {
            self.server.admin_listen = v;
        }
        if let Some(v) = get("TOKENREIN_REDIS_URL") {
            self.redis.url = v;
        }
        if let Some(v) = get("TOKENREIN_PUBLIC_URL") {
            self.auth.public_url = Some(v.trim().trim_end_matches('/').to_string());
        }
        if let Some(v) = get("TOKENREIN_HISTORY_DIR") {
            self.history.dir = Some(v.trim().to_string());
        }
        if let Some(n) = get("TOKENREIN_HISTORY_RETENTION_DAYS").and_then(|v| v.trim().parse().ok())
        {
            self.history.retention_days = n;
        }
        if let Some(n) =
            get("TOKENREIN_DATABASE_MAX_CONNECTIONS").and_then(|v| v.trim().parse().ok())
        {
            self.database.max_connections = n;
        }
    }
}

/// Looks up environment variables (provider keys, admin token); swapped out in tests.
pub type EnvFn = Arc<dyn Fn(&str) -> Option<String> + Send + Sync>;

/// The process environment, where any variable `NAME` can instead be given as `NAME_FILE`: the
/// path of a file holding the value (Docker and Kubernetes secrets). `NAME` wins if both are set.
pub fn process_env(name: &str) -> Option<String> {
    env_or_file(name, |n| std::env::var(n).ok()).or_else(|| legacy_env(name))
}

/// Before the product was renamed, server variables started with `TL_` (e.g. `TL_ADMIN_TOKEN`).
/// They are still read, with a warning, until v1.1.
fn legacy_env(name: &str) -> Option<String> {
    static WARNED: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());
    let old = format!("TL_{}", name.strip_prefix("TOKENREIN_")?);
    let value = env_or_file(&old, |n| std::env::var(n).ok())?;
    let mut warned = WARNED.lock().unwrap_or_else(|e| e.into_inner());
    if !warned.contains(&old) {
        tracing::warn!(old = %old, new = %name, "deprecated environment variable; rename it before v1.1");
        warned.push(old);
    }
    Some(value)
}

fn env_or_file(name: &str, var: impl Fn(&str) -> Option<String>) -> Option<String> {
    if let Some(v) = var(name) {
        return Some(v);
    }
    let path = var(&format!("{name}_FILE")).filter(|p| !p.is_empty())?;
    match std::fs::read_to_string(&path) {
        // Secret files usually end with a newline; values never do.
        Ok(v) => Some(v.trim_end_matches(['\n', '\r']).to_string()),
        Err(e) => {
            tracing::error!(var = %format!("{name}_FILE"), path, error = %e, "cannot read secret file");
            None
        }
    }
}

pub struct Resolved {
    pub cfg: Config,
    pub keys: HashMap<String, (usize, usize)>,
    provider_keys: HashMap<(usize, Provider), String>,
    provider_status: HashMap<(usize, Provider), ProviderKeyStatus>,
    /// Human-readable list of org/provider pairs with no usable key.
    pub missing_provider_keys: Vec<String>,
    pub admin_token: Option<String>,
    /// Postgres connection URL, if one is configured.
    pub database_url: Option<String>,
    /// Model prices: built-in list plus `cfg.pricing` overrides.
    pub prices: crate::pricing::Catalog,
    secrets: Option<Arc<SecretBox>>,
    previous_secrets: Option<Arc<SecretBox>>,
    env: EnvFn,
}

impl Resolved {
    pub fn load(
        path: &str,
        env: impl Fn(&str) -> Option<String> + Send + Sync + 'static,
    ) -> Result<Self> {
        let text =
            std::fs::read_to_string(path).with_context(|| format!("reading config {path}"))?;
        Self::from_toml(&text, env)
    }

    pub fn from_toml(
        text: &str,
        env: impl Fn(&str) -> Option<String> + Send + Sync + 'static,
    ) -> Result<Self> {
        let mut cfg: Config = toml::from_str(text).context("parsing config")?;
        cfg.apply_env_overrides(&env);
        Self::build(cfg, Arc::new(env))
    }

    /// The same server settings with a different set of orgs (from the admin API / dashboard).
    /// Validates exactly like the config file.
    pub fn with_orgs(&self, orgs: Vec<OrgConfig>) -> Result<Self> {
        self.with_managed(orgs, self.cfg.pricing.clone())
    }

    /// Orgs and price overrides managed from the admin API / dashboard.
    pub fn with_managed(&self, orgs: Vec<OrgConfig>, pricing: Vec<PriceOverride>) -> Result<Self> {
        let mut cfg = self.cfg.clone();
        cfg.orgs = orgs;
        cfg.pricing = pricing;
        Self::build(cfg, self.env.clone())
    }

    fn build(cfg: Config, env: EnvFn) -> Result<Self> {
        if !(1..=3650).contains(&cfg.history.retention_days) {
            bail!("history.retention_days must be 1-3650");
        }
        if !(1..=3600).contains(&cfg.history.flush_secs) {
            bail!("history.flush_secs must be 1-3600");
        }
        let mut keys = HashMap::new();
        let mut org_ids = HashSet::new();
        for (oi, org) in cfg.orgs.iter().enumerate() {
            ensure_id("org", &org.id)?;
            if !org_ids.insert(org.id.as_str()) {
                bail!("duplicate org id {:?}", org.id);
            }
            validate_budgets(&org.budgets, &org.id)?;
            if let Some(ws) = &org.anthropic_workspace_id
                && !valid_id(ws)
            {
                bail!(
                    "org {:?}: anthropic_workspace_id must be 1-128 letters, digits, '.', '_' or '-'",
                    org.id
                );
            }
            for name in org.provider_key_env.keys() {
                if Provider::from_name(name).is_none() {
                    bail!(
                        "org {:?}: unknown provider {name:?} in provider_key_env",
                        org.id
                    );
                }
            }
            let mut project_ids = HashSet::new();
            for (pi, project) in org.projects.iter().enumerate() {
                ensure_id("project", &project.id)?;
                if !project_ids.insert(project.id.as_str()) {
                    bail!("org {:?}: duplicate project id {:?}", org.id, project.id);
                }
                validate_budgets(&project.budgets, &project.id)?;
                for k in &project.keys {
                    if k.name.len() > 100 || k.prefix.len() > 20 {
                        bail!("{}/{}: key name or prefix too long", org.id, project.id);
                    }
                }
                for hash in project.key_hashes() {
                    let hash = hash.to_ascii_lowercase();
                    if hash.len() != 64 || !hash.bytes().all(|b| b.is_ascii_hexdigit()) {
                        bail!(
                            "{}/{}: virtual key hashes must be 64 hex characters (sha256)",
                            org.id,
                            project.id
                        );
                    }
                    if keys.insert(hash, (oi, pi)).is_some() {
                        bail!("{}/{}: virtual key hash is used twice", org.id, project.id);
                    }
                }
            }
        }

        let secrets_box = |var: &str| -> Result<Option<Arc<SecretBox>>> {
            match env(var) {
                Some(k) if !k.trim().is_empty() => Ok(Some(Arc::new(
                    SecretBox::from_hex(&k).with_context(|| format!("invalid {var}"))?,
                ))),
                _ => Ok(None),
            }
        };
        let secrets = secrets_box(&cfg.server.secrets_key_env)?;
        let previous_secrets = secrets_box(&format!("{}_PREVIOUS", cfg.server.secrets_key_env))?;
        if previous_secrets.is_some() && secrets.is_none() {
            bail!(
                "{0}_PREVIOUS is set but {0} is not",
                cfg.server.secrets_key_env
            );
        }

        let mut provider_keys = HashMap::new();
        let mut provider_status = HashMap::new();
        let mut missing = Vec::new();
        for (provider, pc) in [
            (Provider::OpenAI, &cfg.providers.openai),
            (Provider::Anthropic, &cfg.providers.anthropic),
        ] {
            for (oi, org) in cfg.orgs.iter().enumerate() {
                let mut status = ProviderKeyStatus {
                    configured: pc.is_some(),
                    source: None,
                    env_var: None,
                    last4: None,
                    updated_at: None,
                    error: None,
                };
                if let Some(pc) = pc {
                    let var = org
                        .provider_key_env
                        .get(provider.name())
                        .unwrap_or(&pc.api_key_env);
                    status.env_var = Some(var.clone());
                    if let Some(v) = env(var).filter(|v| !v.is_empty()) {
                        provider_keys.insert((oi, provider), v);
                        status.source = Some("environment");
                    }
                    if let Some(saved) = org.provider_secrets.get(provider.name()) {
                        let opened = match &secrets {
                            None => Err(format!(
                                "saved key cannot be decrypted: {} is not set",
                                cfg.server.secrets_key_env
                            )),
                            Some(b) => {
                                let ctx = crate::secrets::context(&org.id, provider.name());
                                b.open(&ctx, &saved.sealed)
                                    .or_else(|e| match &previous_secrets {
                                        Some(p) => p.open(&ctx, &saved.sealed),
                                        None => Err(e),
                                    })
                                    .map_err(|e| format!("saved key cannot be used: {e}"))
                            }
                        };
                        status.last4 = Some(saved.last4.clone());
                        status.updated_at = Some(saved.updated_at.clone());
                        match opened {
                            Ok(key) => {
                                provider_keys.insert((oi, provider), key);
                                status.source = Some("dashboard");
                            }
                            Err(e) => status.error = Some(e),
                        }
                    }
                    if status.source.is_none() {
                        missing.push(format!(
                            "{} key for org {} (set {var} or save one in the dashboard)",
                            provider.name(),
                            org.id
                        ));
                    }
                }
                provider_status.insert((oi, provider), status);
            }
            for name in cfg.orgs.iter().flat_map(|o| o.provider_secrets.keys()) {
                if Provider::from_name(name).is_none() {
                    bail!("unknown provider {name:?} in provider_secrets");
                }
            }
        }

        let admin_token = env(&cfg.server.admin_token_env).filter(|t| t.len() >= 16);
        let database_url = env(&cfg.database.url_env).filter(|u| !u.trim().is_empty());
        let prices = crate::pricing::Catalog::with_overrides(&cfg.pricing)?;
        Ok(Self {
            prices,
            cfg,
            keys,
            provider_keys,
            provider_status,
            missing_provider_keys: missing,
            admin_token,
            database_url,
            secrets,
            previous_secrets,
            env,
        })
    }

    pub fn provider_key(&self, org_idx: usize, provider: Provider) -> Option<&str> {
        self.provider_keys
            .get(&(org_idx, provider))
            .map(String::as_str)
    }

    pub fn provider_status(
        &self,
        org_idx: usize,
        provider: Provider,
    ) -> Option<&ProviderKeyStatus> {
        self.provider_status.get(&(org_idx, provider))
    }

    /// Encrypts provider keys saved from the dashboard; None when the secrets key is not set.
    /// An environment variable, through the same lookup the config uses.
    pub fn env_var(&self, name: &str) -> Option<String> {
        (self.env)(name)
    }

    pub fn secrets(&self) -> Option<&SecretBox> {
        self.secrets.as_deref()
    }

    /// The secrets key being rotated away from, if one is set.
    pub fn previous_secrets(&self) -> Option<&SecretBox> {
        self.previous_secrets.as_deref()
    }

    pub fn upstream_base(&self, provider: Provider) -> Option<&str> {
        let pc = match provider {
            Provider::OpenAI => self.cfg.providers.openai.as_ref(),
            Provider::Anthropic => self.cfg.providers.anthropic.as_ref(),
        };
        pc.map(|c| c.base_url.trim_end_matches('/'))
    }
}

pub fn valid_id(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 128
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// Users may be emails: ids plus '@' and '+'.
pub fn valid_user(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 128
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-' | b'@' | b'+'))
}

fn ensure_id(kind: &str, s: &str) -> Result<()> {
    if valid_id(s) {
        Ok(())
    } else {
        bail!("invalid {kind} id {s:?}: use 1-128 letters, digits, '.', '_' or '-'")
    }
}

fn validate_budgets(b: &Budgets, owner: &str) -> Result<()> {
    for policy in [b.org_monthly, b.project_monthly, b.agent_daily, b.run]
        .into_iter()
        .flatten()
    {
        if !(policy.limit_usd.is_finite() && policy.limit_usd >= 0.0) {
            bail!("{owner}: budget limits must be zero or positive");
        }
    }
    // An approval gives one run more budget; there is no "more" for a shared daily or monthly
    // budget without changing it for everyone.
    for policy in [b.org_monthly, b.project_monthly, b.agent_daily]
        .into_iter()
        .flatten()
    {
        if policy.mode == Mode::Ask {
            bail!("{owner}: \"ask\" mode is for run budgets; use enforce or check here");
        }
    }
    if let Some(l) = b.loops {
        if l.max_repeats < 2 || l.repeat_window_secs < 10 || l.repeat_window_secs > 86_400 {
            bail!("{owner}: loop max_repeats must be at least 2 and repeat_window_secs 10-86400");
        }
        if l.max_usd_per_minute
            .is_some_and(|v| !(v.is_finite() && v > 0.0))
            || l.max_calls_per_run == Some(0)
        {
            bail!("{owner}: loop limits must be positive");
        }
    }
    if let Some(max) = b.max_run_usd
        && !(max.is_finite() && max > 0.0)
    {
        bail!("{owner}: max_run_usd must be positive");
    }
    Ok(())
}

pub fn sha256_hex(s: &str) -> String {
    hex::encode(Sha256::digest(s.as_bytes()))
}

pub fn generate_virtual_key() -> (String, String) {
    let key = format!(
        "tr_{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    );
    let hash = sha256_hex(&key);
    (key, hash)
}

pub fn usd_to_micros(usd: f64) -> u64 {
    (usd * 1_000_000.0).round() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(name: &str) -> Option<String> {
        (name == "OPENAI_API_KEY").then(|| "sk-test".to_string())
    }

    const BASE: &str = r#"
        [server]
        listen = "127.0.0.1:0"
        [redis]
        url = "redis://127.0.0.1:6379"
        [providers.openai]
        base_url = "https://api.openai.com/"
        api_key_env = "OPENAI_API_KEY"
        [[orgs]]
        id = "acme"
        [orgs.budgets]
        run = { limit_usd = 1.0, mode = "enforce" }
        agent_daily = { limit_usd = 10.0 }
        [[orgs.projects]]
        id = "bot"
        virtual_key_hashes = ["9CBE3BABAAD4BEB8958EB61B5459917880673DC29DEC664C1B40D6A689B2B55A"]
        [orgs.projects.budgets]
        run = { limit_usd = 0.5, mode = "check" }
    "#;

    #[test]
    fn loads_and_indexes_keys() {
        let rt = Resolved::from_toml(BASE, env).unwrap();
        let hash = sha256_hex("tr_dev_support_bot_key_change_me");
        assert_eq!(rt.keys.get(&hash), Some(&(0, 0)));
        assert_eq!(
            rt.upstream_base(Provider::OpenAI),
            Some("https://api.openai.com")
        );
        assert_eq!(rt.provider_key(0, Provider::OpenAI), Some("sk-test"));
    }

    #[test]
    fn project_overrides_org_and_mode_defaults_to_check() {
        let rt = Resolved::from_toml(BASE, env).unwrap();
        let org = &rt.cfg.orgs[0];
        let eff = Budgets::effective(&org.budgets, &org.projects[0].budgets);
        assert_eq!(eff.run.unwrap().limit_usd, 0.5);
        assert_eq!(eff.run.unwrap().mode, Mode::Check);
        assert_eq!(eff.agent_daily.unwrap().mode, Mode::Check);
    }

    #[test]
    fn missing_provider_key_is_reported_not_fatal() {
        // The proxy must start without provider keys so they can be added from the dashboard.
        let rt = Resolved::from_toml(BASE, |_| None).unwrap();
        assert_eq!(rt.provider_key(0, Provider::OpenAI), None);
        assert!(rt.missing_provider_keys[0].contains("OPENAI_API_KEY"));
        assert_eq!(
            rt.provider_status(0, Provider::OpenAI).unwrap().source,
            None
        );
    }

    #[test]
    fn saved_provider_keys_override_the_environment() {
        const SK: &str = "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f";
        let with_secret = |name: &str| match name {
            "OPENAI_API_KEY" => Some("sk-env".to_string()),
            "TOKENREIN_SECRETS_KEY" => Some(SK.to_string()),
            _ => None,
        };
        let rt = Resolved::from_toml(BASE, with_secret).unwrap();
        let mut orgs = rt.cfg.orgs.clone();
        let sealed = rt
            .secrets()
            .unwrap()
            .seal(&crate::secrets::context("acme", "openai"), "sk-dashboard")
            .unwrap();
        orgs[0].provider_secrets.insert(
            "openai".into(),
            ProviderSecret {
                sealed,
                last4: "oard".into(),
                updated_at: "now".into(),
            },
        );
        let rt = rt.with_orgs(orgs.clone()).unwrap();
        assert_eq!(rt.provider_key(0, Provider::OpenAI), Some("sk-dashboard"));
        assert_eq!(
            rt.provider_status(0, Provider::OpenAI).unwrap().source,
            Some("dashboard")
        );

        // Without the secrets key the saved key is unusable; the environment key is used.
        let no_secret = Resolved::from_toml(BASE, env)
            .unwrap()
            .with_orgs(orgs)
            .unwrap();
        assert_eq!(no_secret.provider_key(0, Provider::OpenAI), Some("sk-test"));
        let st = no_secret.provider_status(0, Provider::OpenAI).unwrap();
        assert_eq!(st.source, Some("environment"));
        assert!(
            st.error
                .as_deref()
                .unwrap()
                .contains("TOKENREIN_SECRETS_KEY")
        );
    }

    #[test]
    fn provider_keys_must_be_header_safe() {
        assert!(valid_provider_key("sk-ant-api03-abcdefgh"));
        assert!(!valid_provider_key("short"));
        assert!(!valid_provider_key("sk-ant-abc def"));
        assert!(!valid_provider_key("sk-ant-abc\r\nX-Evil: 1"));
    }

    #[test]
    fn env_overrides_server_and_redis_settings() {
        let rt = Resolved::from_toml(BASE, |name| match name {
            "TOKENREIN_LISTEN" => Some("0.0.0.0:8787".into()),
            "TOKENREIN_ADMIN_LISTEN" => Some("0.0.0.0:8788".into()),
            "TOKENREIN_REDIS_URL" => Some("redis://redis:6379/2".into()),
            "TOKENREIN_DATABASE_MAX_CONNECTIONS" => Some("25".into()),
            "TOKENREIN_PUBLIC_URL" => Some("https://tokenrein.example.com/".into()),
            _ => None,
        })
        .unwrap();
        assert_eq!(rt.cfg.server.listen, "0.0.0.0:8787");
        assert_eq!(rt.cfg.server.admin_listen, "0.0.0.0:8788");
        assert_eq!(rt.cfg.redis.url, "redis://redis:6379/2");
        assert_eq!(rt.cfg.database.max_connections, 25);
        let public = rt.cfg.auth.public_url.as_deref();
        assert_eq!(public, Some("https://tokenrein.example.com"));
        // Empty values keep the file's settings.
        let rt = Resolved::from_toml(BASE, |name| (name == "TOKENREIN_LISTEN").then(String::new))
            .unwrap();
        assert_eq!(rt.cfg.server.listen, "127.0.0.1:0");
    }

    #[test]
    fn values_can_come_from_files() {
        let dir = std::env::temp_dir().join(format!("tl-env-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("token");
        std::fs::write(&file, "from-the-file\n").unwrap();
        let path = file.to_string_lossy().to_string();
        let vars = |direct: Option<&str>| {
            let path = path.clone();
            let direct = direct.map(str::to_string);
            move |n: &str| match n {
                "TOKENREIN_ADMIN_TOKEN" => direct.clone(),
                "TOKENREIN_ADMIN_TOKEN_FILE" => Some(path.clone()),
                "TOKENREIN_MISSING_FILE" => Some("/nonexistent/tokenrein".into()),
                _ => None,
            }
        };
        assert_eq!(
            env_or_file("TOKENREIN_ADMIN_TOKEN", vars(None)).as_deref(),
            Some("from-the-file")
        );
        // The variable itself wins over the file.
        assert_eq!(
            env_or_file("TOKENREIN_ADMIN_TOKEN", vars(Some("direct"))).as_deref(),
            Some("direct")
        );
        assert_eq!(env_or_file("TOKENREIN_MISSING", vars(None)), None);
        assert_eq!(env_or_file("TOKENREIN_UNSET", vars(None)), None);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn ids_are_restricted() {
        assert!(valid_id("run-42.a_b"));
        assert!(!valid_id("run:42"));
        assert!(!valid_id("{acme}"));
        assert!(!valid_id(""));
    }
}
