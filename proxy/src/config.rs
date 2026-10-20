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
}

impl Mode {
    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Skip => "skip",
            Mode::Check => "check",
            Mode::Enforce => "enforce",
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
    #[serde(default = "default_secrets_key_env")]
    pub secrets_key_env: String,
    #[serde(default = "default_upstream_timeout")]
    pub upstream_timeout_secs: u64,
    #[serde(default = "default_max_output")]
    pub default_max_output_tokens: u64,
    #[serde(default = "default_max_body")]
    pub max_body_bytes: usize,
}

fn default_admin_listen() -> String {
    "127.0.0.1:8788".into()
}
fn default_admin_token_env() -> String {
    "TL_ADMIN_TOKEN".into()
}
fn default_secrets_key_env() -> String {
    "TL_SECRETS_KEY".into()
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

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RedisConfig {
    pub url: String,
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

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PriceOverride {
    pub provider: String,
    pub model: String,
    pub input_per_mtok: f64,
    pub output_per_mtok: f64,
    pub cached_input_per_mtok: Option<f64>,
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
    #[serde(default)]
    pub projects: Vec<ProjectConfig>,
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
    pub providers: ProvidersConfig,
    #[serde(default)]
    pub pricing: Vec<PriceOverride>,
    #[serde(default)]
    pub orgs: Vec<OrgConfig>,
}

/// Looks up environment variables (provider keys, admin token); swapped out in tests.
pub type EnvFn = Arc<dyn Fn(&str) -> Option<String> + Send + Sync>;

pub struct Resolved {
    pub cfg: Config,
    pub keys: HashMap<String, (usize, usize)>,
    provider_keys: HashMap<(usize, Provider), String>,
    provider_status: HashMap<(usize, Provider), ProviderKeyStatus>,
    /// Human-readable list of org/provider pairs with no usable key.
    pub missing_provider_keys: Vec<String>,
    pub admin_token: Option<String>,
    secrets: Option<Arc<SecretBox>>,
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
        let cfg: Config = toml::from_str(text).context("parsing config")?;
        Self::build(cfg, Arc::new(env))
    }

    /// The same server settings with a different set of orgs (from the admin API / dashboard).
    /// Validates exactly like the config file.
    pub fn with_orgs(&self, orgs: Vec<OrgConfig>) -> Result<Self> {
        let mut cfg = self.cfg.clone();
        cfg.orgs = orgs;
        Self::build(cfg, self.env.clone())
    }

    fn build(cfg: Config, env: EnvFn) -> Result<Self> {
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

        let secrets = match env(&cfg.server.secrets_key_env) {
            Some(k) if !k.trim().is_empty() => {
                Some(Arc::new(SecretBox::from_hex(&k).with_context(|| {
                    format!("invalid {}", cfg.server.secrets_key_env)
                })?))
            }
            _ => None,
        };

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
                            Some(b) => b
                                .open(
                                    &crate::secrets::context(&org.id, provider.name()),
                                    &saved.sealed,
                                )
                                .map_err(|e| format!("saved key cannot be used: {e}")),
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
        Ok(Self {
            cfg,
            keys,
            provider_keys,
            provider_status,
            missing_provider_keys: missing,
            admin_token,
            secrets,
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
    pub fn secrets(&self) -> Option<&SecretBox> {
        self.secrets.as_deref()
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
        "tl_{}{}",
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
        virtual_key_hashes = ["EFECD1D2D2C5A6C4F93F91CE03F782928BD1301C1A243EF8894FA84E868B285E"]
        [orgs.projects.budgets]
        run = { limit_usd = 0.5, mode = "check" }
    "#;

    #[test]
    fn loads_and_indexes_keys() {
        let rt = Resolved::from_toml(BASE, env).unwrap();
        let hash = sha256_hex("tl_dev_support_bot_key_change_me");
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
            "TL_SECRETS_KEY" => Some(SK.to_string()),
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
        assert!(st.error.as_deref().unwrap().contains("TL_SECRETS_KEY"));
    }

    #[test]
    fn provider_keys_must_be_header_safe() {
        assert!(valid_provider_key("sk-ant-api03-abcdefgh"));
        assert!(!valid_provider_key("short"));
        assert!(!valid_provider_key("sk-ant-abc def"));
        assert!(!valid_provider_key("sk-ant-abc\r\nX-Evil: 1"));
    }

    #[test]
    fn ids_are_restricted() {
        assert!(valid_id("run-42.a_b"));
        assert!(!valid_id("run:42"));
        assert!(!valid_id("{acme}"));
        assert!(!valid_id(""));
    }
}
