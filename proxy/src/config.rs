use std::collections::{HashMap, HashSet};

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::providers::Provider;

#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
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

#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum FailMode {
    #[default]
    Open,
    Closed,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BudgetPolicy {
    pub limit_usd: f64,
    #[serde(default)]
    pub mode: Mode,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Budgets {
    pub org_monthly: Option<BudgetPolicy>,
    pub project_monthly: Option<BudgetPolicy>,
    pub agent_daily: Option<BudgetPolicy>,
    pub run: Option<BudgetPolicy>,
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

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectConfig {
    pub id: String,
    #[serde(default)]
    pub virtual_key_hashes: Vec<String>,
    #[serde(default)]
    pub budgets: Budgets,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OrgConfig {
    pub id: String,
    #[serde(default)]
    pub fail_mode: FailMode,
    #[serde(default)]
    pub budgets: Budgets,
    #[serde(default)]
    pub provider_key_env: HashMap<String, String>,
    #[serde(default)]
    pub projects: Vec<ProjectConfig>,
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

pub struct Resolved {
    pub cfg: Config,
    pub keys: HashMap<String, (usize, usize)>,
    provider_keys: HashMap<(usize, Provider), String>,
    pub admin_token: Option<String>,
}

impl Resolved {
    pub fn load(path: &str, env: impl Fn(&str) -> Option<String>) -> Result<Self> {
        let text =
            std::fs::read_to_string(path).with_context(|| format!("reading config {path}"))?;
        Self::from_toml(&text, env)
    }

    pub fn from_toml(text: &str, env: impl Fn(&str) -> Option<String>) -> Result<Self> {
        let cfg: Config = toml::from_str(text).context("parsing config")?;

        let mut keys = HashMap::new();
        let mut org_ids = HashSet::new();
        for (oi, org) in cfg.orgs.iter().enumerate() {
            ensure_id("org", &org.id)?;
            if !org_ids.insert(org.id.as_str()) {
                bail!("duplicate org id {:?}", org.id);
            }
            validate_budgets(&org.budgets, &org.id)?;
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
                for hash in &project.virtual_key_hashes {
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

        let mut provider_keys = HashMap::new();
        let mut missing = Vec::new();
        for (provider, pc) in [
            (Provider::OpenAI, &cfg.providers.openai),
            (Provider::Anthropic, &cfg.providers.anthropic),
        ] {
            let Some(pc) = pc else { continue };
            for (oi, org) in cfg.orgs.iter().enumerate() {
                let var = org
                    .provider_key_env
                    .get(provider.name())
                    .unwrap_or(&pc.api_key_env);
                match env(var) {
                    Some(v) if !v.is_empty() => {
                        provider_keys.insert((oi, provider), v);
                    }
                    _ => missing.push(format!(
                        "{var} ({} key for org {})",
                        provider.name(),
                        org.id
                    )),
                }
            }
        }
        if !missing.is_empty() {
            bail!(
                "missing provider API keys in the environment: {}",
                missing.join(", ")
            );
        }

        let admin_token = env(&cfg.server.admin_token_env).filter(|t| t.len() >= 16);
        Ok(Self {
            cfg,
            keys,
            provider_keys,
            admin_token,
        })
    }

    pub fn provider_key(&self, org_idx: usize, provider: Provider) -> Option<&str> {
        self.provider_keys
            .get(&(org_idx, provider))
            .map(String::as_str)
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
    fn missing_provider_key_fails_fast() {
        let err = Resolved::from_toml(BASE, |_| None)
            .err()
            .unwrap()
            .to_string();
        assert!(err.contains("OPENAI_API_KEY"), "{err}");
    }

    #[test]
    fn ids_are_restricted() {
        assert!(valid_id("run-42.a_b"));
        assert!(!valid_id("run:42"));
        assert!(!valid_id("{acme}"));
        assert!(!valid_id(""));
    }
}
