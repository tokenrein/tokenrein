use anyhow::{Result, bail};
use serde::Serialize;

use crate::config::{PriceOverride, usd_to_micros, valid_id};
use crate::providers::{Provider, Usage};

/// Prices in micro-USD per million tokens.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Price {
    pub input: u64,
    pub output: u64,
    pub cached_input: u64,
    pub cache_write: u64,
}

impl Price {
    pub fn usage_cost(&self, u: &Usage) -> u64 {
        tokens_cost(u.input, self.input)
            + tokens_cost(u.cached_input, self.cached_input)
            + tokens_cost(u.cache_write, self.cache_write)
            + tokens_cost(u.output, self.output)
    }
}

pub fn tokens_cost(tokens: u64, micros_per_mtok: u64) -> u64 {
    (tokens as u128 * micros_per_mtok as u128).div_ceil(1_000_000) as u64
}

/// Month the built-in list prices were last checked; override them (config `[[pricing]]` or the
/// dashboard's Models & prices page) when they change.
pub const BUILTIN_VERIFIED_AS_OF: &str = "2026-10";

/// USD per million tokens: input, output, cached input, cache write.
const fn usd(input: f64, output: f64, cached: f64, write: f64) -> Price {
    Price {
        input: (input * 1_000_000.0 + 0.5) as u64,
        output: (output * 1_000_000.0 + 0.5) as u64,
        cached_input: (cached * 1_000_000.0 + 0.5) as u64,
        cache_write: (write * 1_000_000.0 + 0.5) as u64,
    }
}

/// Built-in list prices: (provider, model prefix, price). A prefix matches the model itself and
/// any dated or tagged variant (`gpt-4o-2024-08-06`, `claude-haiku-4-5-20251001`, `llama3:8b`).
const BUILTIN: &[(&str, &str, Price)] = &[
    // OpenAI
    ("openai", "gpt-4o", usd(2.5, 10.0, 1.25, 2.5)),
    ("openai", "gpt-4o-mini", usd(0.15, 0.6, 0.075, 0.15)),
    ("openai", "gpt-4.1", usd(2.0, 8.0, 0.5, 2.0)),
    ("openai", "gpt-4.1-mini", usd(0.4, 1.6, 0.1, 0.4)),
    ("openai", "gpt-4.1-nano", usd(0.1, 0.4, 0.025, 0.1)),
    ("openai", "gpt-5", usd(1.25, 10.0, 0.125, 1.25)),
    ("openai", "gpt-5-mini", usd(0.25, 2.0, 0.025, 0.25)),
    ("openai", "gpt-5-nano", usd(0.05, 0.4, 0.005, 0.05)),
    ("openai", "o1", usd(15.0, 60.0, 7.5, 15.0)),
    ("openai", "o1-mini", usd(1.1, 4.4, 0.55, 1.1)),
    ("openai", "o3", usd(2.0, 8.0, 0.5, 2.0)),
    ("openai", "o3-mini", usd(1.1, 4.4, 0.55, 1.1)),
    ("openai", "o4-mini", usd(1.1, 4.4, 0.275, 1.1)),
    ("openai", "gpt-oss-120b", usd(0.15, 0.75, 0.15, 0.15)),
    ("openai", "gpt-oss-20b", usd(0.1, 0.5, 0.1, 0.1)),
    // Anthropic (first-party API rates)
    ("anthropic", "claude-fable-5-1", usd(10.0, 50.0, 0.25, 12.5)),
    ("anthropic", "claude-fable-5", usd(10.0, 50.0, 1.0, 12.5)),
    (
        "anthropic",
        "claude-mythos-5-1",
        usd(10.0, 50.0, 0.25, 12.5),
    ),
    ("anthropic", "claude-opus-5-5", usd(4.0, 20.0, 0.2, 5.0)),
    ("anthropic", "claude-opus-5", usd(5.0, 25.0, 0.5, 6.25)),
    ("anthropic", "claude-opus-4-8", usd(5.0, 25.0, 0.5, 6.25)),
    ("anthropic", "claude-opus-4-7", usd(5.0, 25.0, 0.5, 6.25)),
    ("anthropic", "claude-opus-4-6", usd(5.0, 25.0, 0.5, 6.25)),
    ("anthropic", "claude-opus-4-5", usd(5.0, 25.0, 0.5, 6.25)),
    ("anthropic", "claude-opus-4", usd(15.0, 75.0, 1.5, 18.75)),
    ("anthropic", "claude-sonnet-5-5", usd(2.0, 10.0, 0.2, 2.5)),
    ("anthropic", "claude-sonnet-5", usd(2.0, 10.0, 0.2, 2.5)),
    ("anthropic", "claude-sonnet-4", usd(3.0, 15.0, 0.3, 3.75)),
    ("anthropic", "claude-3-7-sonnet", usd(3.0, 15.0, 0.3, 3.75)),
    ("anthropic", "claude-3-5-sonnet", usd(3.0, 15.0, 0.3, 3.75)),
    ("anthropic", "claude-haiku-5-5", usd(0.1, 0.5, 0.01, 0.125)),
    ("anthropic", "claude-haiku-4-5", usd(1.0, 5.0, 0.1, 1.25)),
    ("anthropic", "claude-3-5-haiku", usd(0.8, 4.0, 0.08, 1.0)),
    ("anthropic", "claude-3-haiku", usd(0.25, 1.25, 0.03, 0.3)),
    // Google
    ("google", "gemini-3-pro", usd(2.0, 12.0, 0.2, 2.0)),
    ("google", "gemini-2.5-pro", usd(1.25, 10.0, 0.31, 1.25)),
    ("google", "gemini-2.5-flash", usd(0.3, 2.5, 0.075, 0.3)),
    ("google", "gemini-2.5-flash-lite", usd(0.1, 0.4, 0.025, 0.1)),
    ("google", "gemini-2.0-flash", usd(0.1, 0.4, 0.025, 0.1)),
    (
        "google",
        "gemini-2.0-flash-lite",
        usd(0.075, 0.3, 0.075, 0.075),
    ),
    // Mistral
    ("mistral", "mistral-large", usd(2.0, 6.0, 2.0, 2.0)),
    ("mistral", "mistral-medium", usd(0.4, 2.0, 0.4, 0.4)),
    ("mistral", "mistral-small", usd(0.1, 0.3, 0.1, 0.1)),
    ("mistral", "magistral-medium", usd(2.0, 5.0, 2.0, 2.0)),
    ("mistral", "codestral", usd(0.3, 0.9, 0.3, 0.3)),
    ("mistral", "devstral", usd(0.1, 0.3, 0.1, 0.1)),
    // DeepSeek
    ("deepseek", "deepseek-chat", usd(0.28, 0.42, 0.028, 0.28)),
    (
        "deepseek",
        "deepseek-reasoner",
        usd(0.28, 0.42, 0.028, 0.28),
    ),
    // xAI
    ("xai", "grok-4", usd(3.0, 15.0, 0.75, 3.0)),
    ("xai", "grok-4-fast", usd(0.2, 0.5, 0.05, 0.2)),
    ("xai", "grok-code-fast", usd(0.2, 1.5, 0.02, 0.2)),
    ("xai", "grok-3", usd(3.0, 15.0, 0.75, 3.0)),
    ("xai", "grok-3-mini", usd(0.3, 0.5, 0.075, 0.3)),
    // Groq-hosted open models
    (
        "groq",
        "llama-3.3-70b-versatile",
        usd(0.59, 0.79, 0.59, 0.59),
    ),
    ("groq", "llama-3.1-8b-instant", usd(0.05, 0.08, 0.05, 0.05)),
    // Cohere
    ("cohere", "command-a", usd(2.5, 10.0, 2.5, 2.5)),
    ("cohere", "command-r-plus", usd(2.5, 10.0, 2.5, 2.5)),
    ("cohere", "command-r", usd(0.15, 0.6, 0.15, 0.15)),
];

/// Models served on your own hardware cost nothing per token.
const LOCAL_PROVIDERS: &[&str] = &["ollama", "vllm", "lmstudio", "llamacpp", "local"];

/// Other names a provider is known by (SDK base URLs, cloud resellers).
fn canonical_provider(p: &str) -> String {
    let p = p.trim().to_ascii_lowercase();
    match p.as_str() {
        "gemini" | "vertex" | "vertexai" | "google-ai" => "google".into(),
        "grok" | "x-ai" => "xai".into(),
        "azure" | "azure-openai" => "openai".into(),
        "bedrock" | "claude" => "anthropic".into(),
        "mistralai" => "mistral".into(),
        _ => p,
    }
}

/// Lowercase and drop reseller prefixes: `us.anthropic.claude-…`, `anthropic/claude-…`,
/// `models/gemini-…`, `openai/gpt-oss-…`.
fn canonical_model(m: &str) -> String {
    let mut m = m.trim().to_ascii_lowercase();
    for region in ["us.", "eu.", "apac.", "global."] {
        if let Some(rest) = m.strip_prefix(region) {
            m = rest.to_string();
        }
    }
    for vendor in [
        "anthropic.",
        "anthropic/",
        "openai/",
        "google/",
        "models/",
        "x-ai/",
        "mistralai/",
        "deepseek/",
        "meta-llama/",
        "cohere/",
    ] {
        if let Some(rest) = m.strip_prefix(vendor) {
            m = rest.to_string();
            break;
        }
    }
    m
}

#[derive(Debug, Clone)]
struct Entry {
    provider: String,
    prefix: String,
    price: Price,
}

/// One priced model, for the dashboard.
#[derive(Debug, Clone, Serialize)]
pub struct PriceRow {
    pub provider: String,
    pub model: String,
    pub price: Price,
    /// "builtin" or "override".
    pub source: &'static str,
}

pub struct Catalog {
    overrides: Vec<Entry>,
    builtin: Vec<Entry>,
}

impl Catalog {
    pub fn builtin() -> Self {
        Self::with_overrides(&[]).expect("built-in catalog is valid")
    }

    pub fn with_overrides(overrides: &[PriceOverride]) -> Result<Self> {
        let mut parsed = Vec::with_capacity(overrides.len());
        for o in overrides {
            let provider = canonical_provider(&o.provider);
            if !valid_id(&provider) {
                bail!(
                    "pricing override for {:?}: invalid provider {:?}",
                    o.model,
                    o.provider
                );
            }
            if o.model.trim().is_empty() || o.model.len() > 200 {
                bail!("pricing override: model name must be 1-200 characters");
            }
            let values = [
                Some(o.input_per_mtok),
                Some(o.output_per_mtok),
                o.cached_input_per_mtok,
                o.cache_write_per_mtok,
            ];
            if values
                .into_iter()
                .flatten()
                .any(|v| !(v.is_finite() && v >= 0.0))
            {
                bail!(
                    "pricing override for {:?}: prices must be zero or positive",
                    o.model
                );
            }
            let input = usd_to_micros(o.input_per_mtok);
            // Anthropic charges 1.25x input to write the prompt cache; elsewhere writes cost input.
            let default_write = if provider == "anthropic" {
                input + input / 4
            } else {
                input
            };
            parsed.push(Entry {
                provider,
                prefix: canonical_model(&o.model),
                price: Price {
                    input,
                    output: usd_to_micros(o.output_per_mtok),
                    cached_input: o.cached_input_per_mtok.map(usd_to_micros).unwrap_or(input),
                    cache_write: o
                        .cache_write_per_mtok
                        .map(usd_to_micros)
                        .unwrap_or(default_write),
                },
            });
        }
        Ok(Self {
            overrides: parsed,
            builtin: BUILTIN
                .iter()
                .map(|(p, m, price)| Entry {
                    provider: p.to_string(),
                    prefix: m.to_string(),
                    price: *price,
                })
                .collect(),
        })
    }

    /// Returns the price and whether the model was known; unknown models get the provider's most
    /// expensive price, so budgets err on the safe side.
    pub fn lookup(&self, provider: Provider, model: &str) -> (Price, bool) {
        self.lookup_any(provider.name(), model)
    }

    /// Like [`lookup`](Self::lookup) for any provider name ("google", "groq", "ollama", ...).
    /// An empty or unknown provider is matched on the model name alone.
    pub fn lookup_any(&self, provider: &str, model: &str) -> (Price, bool) {
        let provider = canonical_provider(provider);
        if LOCAL_PROVIDERS.contains(&provider.as_str()) {
            return (usd(0.0, 0.0, 0.0, 0.0), true);
        }
        let model = canonical_model(model);
        let scoped = |e: &&Entry| e.provider == provider;
        for entries in [&self.overrides, &self.builtin] {
            if let Some(p) = best_match(entries.iter().filter(scoped), &model) {
                return (p, true);
            }
        }
        // Resellers (OpenRouter, Bedrock, Together, ...) serve other providers' models.
        for entries in [&self.overrides, &self.builtin] {
            if let Some(p) = best_match(entries.iter(), &model) {
                return (p, true);
            }
        }
        (self.fallback(&provider), false)
    }

    fn fallback(&self, provider: &str) -> Price {
        self.builtin
            .iter()
            .chain(&self.overrides)
            .filter(|e| e.provider == provider)
            .map(|e| e.price)
            .max_by_key(|price| price.output)
            .unwrap_or(usd(15.0, 75.0, 15.0, 18.75))
    }

    /// Every priced model: overrides first, then built-in prices they don't replace.
    pub fn rows(&self) -> Vec<PriceRow> {
        let mut rows: Vec<PriceRow> = self
            .overrides
            .iter()
            .map(|e| PriceRow {
                provider: e.provider.clone(),
                model: e.prefix.clone(),
                price: e.price,
                source: "override",
            })
            .collect();
        for e in &self.builtin {
            if !self
                .overrides
                .iter()
                .any(|o| o.provider == e.provider && o.prefix == e.prefix)
            {
                rows.push(PriceRow {
                    provider: e.provider.clone(),
                    model: e.prefix.clone(),
                    price: e.price,
                    source: "builtin",
                });
            }
        }
        rows
    }
}

fn best_match<'a>(entries: impl Iterator<Item = &'a Entry>, model: &str) -> Option<Price> {
    entries
        .filter(|e| matches_prefix(model, &e.prefix))
        .max_by_key(|e| e.prefix.len())
        .map(|e| e.price)
}

fn matches_prefix(model: &str, prefix: &str) -> bool {
    model
        .strip_prefix(prefix)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with(['-', ':', '@']))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn longest_prefix_wins_and_dated_models_match() {
        let c = Catalog::builtin();
        assert_eq!(
            c.lookup(Provider::OpenAI, "gpt-4o-mini-2024-07-18").0.input,
            150_000
        );
        assert_eq!(c.lookup(Provider::OpenAI, "gpt-4o").0.input, 2_500_000);
        assert_eq!(
            c.lookup(Provider::Anthropic, "claude-haiku-4-5-20251001")
                .0
                .output,
            5_000_000
        );
        assert_eq!(
            c.lookup(Provider::Anthropic, "claude-opus-5-5").0.input,
            4_000_000
        );
        assert_eq!(
            c.lookup(Provider::Anthropic, "claude-opus-5").0.input,
            5_000_000
        );
    }

    #[test]
    fn prefix_needs_a_boundary() {
        let c = Catalog::builtin();
        let (_, known) = c.lookup(Provider::OpenAI, "o1x");
        assert!(!known);
    }

    #[test]
    fn unknown_models_fall_back_to_most_expensive() {
        let c = Catalog::builtin();
        let (p, known) = c.lookup(Provider::Anthropic, "claude-future-9");
        assert!(!known);
        assert_eq!(p.output, 75_000_000);
        let (p, known) = c.lookup_any("someprovider", "mystery-model");
        assert!(!known);
        assert_eq!(p.output, 75_000_000);
    }

    #[test]
    fn other_providers_resellers_and_local_models() {
        let c = Catalog::builtin();
        let (p, known) = c.lookup_any("google", "gemini-2.5-flash");
        assert!(known);
        assert_eq!(p.output, 2_500_000);
        // Provider aliases and reseller prefixes.
        assert_eq!(
            c.lookup_any("gemini", "models/gemini-2.5-pro").0.input,
            1_250_000
        );
        assert_eq!(
            c.lookup_any("bedrock", "us.anthropic.claude-sonnet-5-5")
                .0
                .input,
            2_000_000
        );
        assert_eq!(
            c.lookup_any("openrouter", "anthropic/claude-haiku-5-5")
                .0
                .output,
            500_000
        );
        assert_eq!(
            c.lookup_any("groq", "openai/gpt-oss-120b").0.output,
            750_000
        );
        // Model name alone.
        assert!(c.lookup_any("", "deepseek-chat").1);
        // Local models are free.
        let (p, known) = c.lookup_any("ollama", "llama3:8b");
        assert!(known);
        assert_eq!(
            p.usage_cost(&Usage {
                input: 1000,
                output: 1000,
                ..Usage::default()
            }),
            0
        );
    }

    #[test]
    fn overrides_take_precedence_for_any_provider() {
        let o = |provider: &str, model: &str, i: f64, out: f64| PriceOverride {
            provider: provider.into(),
            model: model.into(),
            input_per_mtok: i,
            output_per_mtok: out,
            cached_input_per_mtok: None,
            cache_write_per_mtok: None,
        };
        let c = Catalog::with_overrides(&[
            o("openai", "gpt-4o", 1.0, 2.0),
            o("together", "qwen3-coder", 0.5, 1.5),
        ])
        .unwrap();
        let (p, known) = c.lookup(Provider::OpenAI, "gpt-4o");
        assert!(known);
        assert_eq!(
            (p.input, p.output, p.cached_input),
            (1_000_000, 2_000_000, 1_000_000)
        );
        assert_eq!(
            c.lookup_any("together", "qwen3-coder-480b").0.output,
            1_500_000
        );
        let rows = c.rows();
        assert_eq!(rows[0].source, "override");
        assert!(
            !rows
                .iter()
                .any(|r| r.source == "builtin" && r.model == "gpt-4o" && r.provider == "openai")
        );
        assert!(Catalog::with_overrides(&[o("bad provider", "x", 1.0, 1.0)]).is_err());
        assert!(Catalog::with_overrides(&[o("x", "m", -1.0, 1.0)]).is_err());
    }

    #[test]
    fn cost_rounds_up_to_the_micro() {
        assert_eq!(tokens_cost(1, 150_000), 1);
        assert_eq!(tokens_cost(1_000_000, 150_000), 150_000);
        assert_eq!(tokens_cost(0, 150_000), 0);
    }
}
