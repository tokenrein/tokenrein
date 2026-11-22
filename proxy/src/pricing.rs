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
    /// Writing the prompt cache for the long lifetime (Anthropic's 1 hour, OpenAI's 12 hours).
    pub cache_write_long: u64,
    /// Prompts longer than this many tokens (input, cache reads and writes together) are billed
    /// at the long-context rates below; 0 when the model has one rate.
    #[serde(skip_serializing_if = "is_zero")]
    pub long_context_above: u64,
    /// Long-context rates as a percentage of the normal ones: input-side and output.
    #[serde(skip_serializing_if = "is_zero")]
    pub long_input_pct: u64,
    #[serde(skip_serializing_if = "is_zero")]
    pub long_output_pct: u64,
}

fn is_zero(n: &u64) -> bool {
    *n == 0
}

impl Price {
    pub fn usage_cost(&self, u: &Usage) -> u64 {
        let p = self.for_prompt(u.prompt());
        let long = u.cache_write_long.min(u.cache_write);
        tokens_cost(u.input, p.input)
            + tokens_cost(u.cached_input, p.cached_input)
            + tokens_cost(u.cache_write - long, p.cache_write)
            + tokens_cost(long, p.cache_write_long)
            + tokens_cost(u.output, p.output)
    }

    /// The rates that apply to a prompt of this many tokens.
    pub fn for_prompt(&self, prompt_tokens: u64) -> Price {
        if self.long_context_above == 0 || prompt_tokens <= self.long_context_above {
            return *self;
        }
        let side = |v: u64| v * self.long_input_pct / 100;
        Price {
            input: side(self.input),
            output: self.output * self.long_output_pct / 100,
            cached_input: side(self.cached_input),
            cache_write: side(self.cache_write),
            cache_write_long: side(self.cache_write_long),
            ..*self
        }
    }

    /// The cache-read discount, as tokens' worth of savings: what the cache reads would have
    /// cost as fresh input, minus what they did cost.
    pub fn cache_savings(&self, u: &Usage) -> u64 {
        let p = self.for_prompt(u.prompt());
        tokens_cost(u.cached_input, p.input)
            .saturating_sub(tokens_cost(u.cached_input, p.cached_input))
    }
}

pub fn tokens_cost(tokens: u64, micros_per_mtok: u64) -> u64 {
    (tokens as u128 * micros_per_mtok as u128).div_ceil(1_000_000) as u64
}

/// Month the built-in list prices were last checked; override them (config `[[pricing]]` or the
/// dashboard's Models & prices page) when they change. OpenAI, Anthropic and Google rows were
/// checked against their pricing pages on 2026-11-23.
pub const BUILTIN_VERIFIED_AS_OF: &str = "2026-11";

const fn m(usd: f64) -> u64 {
    (usd * 1_000_000.0 + 0.5) as u64
}

/// USD per million tokens: input, output, cached input, cache write. The long cache write costs
/// the same as a write unless set with [`Price::long_write`].
const fn usd(input: f64, output: f64, cached: f64, write: f64) -> Price {
    Price {
        input: m(input),
        output: m(output),
        cached_input: m(cached),
        cache_write: m(write),
        cache_write_long: m(write),
        long_context_above: 0,
        long_input_pct: 0,
        long_output_pct: 0,
    }
}

/// Anthropic: a 5-minute cache write costs 1.25x input and a 1-hour write 2x.
const fn claude(input: f64, output: f64, cached: f64) -> Price {
    usd(input, output, cached, input * 1.25).long_write(input * 2.0)
}

/// OpenAI from GPT-5.6: a 30-minute cache write costs 1.25x input and a 12-hour write 2.5x.
const fn gpt_cached_writes(input: f64, output: f64, cached: f64) -> Price {
    usd(input, output, cached, input * 1.25).long_write(input * 2.5)
}

impl Price {
    const fn long_write(mut self, usd: f64) -> Price {
        self.cache_write_long = m(usd);
        self
    }

    /// Bill prompts over `above` tokens at `input_pct`% (input, cache) and `output_pct`% (output).
    const fn long_context(mut self, above: u64, input_pct: u64, output_pct: u64) -> Price {
        self.long_context_above = above;
        self.long_input_pct = input_pct;
        self.long_output_pct = output_pct;
        self
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
    ("openai", "gpt-5.4", usd(2.5, 15.0, 0.25, 2.5)),
    ("openai", "gpt-5.5", usd(5.0, 30.0, 0.5, 5.0)),
    ("openai", "gpt-5.6-sol", gpt_cached_writes(4.0, 20.0, 0.4)),
    ("openai", "gpt-6-luna", gpt_cached_writes(0.1, 0.5, 0.01)),
    ("openai", "gpt-6.1-terra", gpt_cached_writes(1.0, 5.0, 0.05)),
    ("openai", "gpt-6.2-sol", gpt_cached_writes(2.0, 10.0, 0.1)),
    ("openai", "gpt-6.2-astra", gpt_cached_writes(8.0, 40.0, 0.4)),
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
    ("anthropic", "claude-fable-5-2", claude(8.0, 40.0, 0.2)),
    ("anthropic", "claude-fable-5-1", claude(10.0, 50.0, 0.25)),
    ("anthropic", "claude-fable-5", claude(10.0, 50.0, 1.0)),
    ("anthropic", "claude-mythos-5-1", claude(10.0, 50.0, 0.25)),
    ("anthropic", "claude-opus-5-5", claude(4.0, 20.0, 0.2)),
    ("anthropic", "claude-opus-5", claude(5.0, 25.0, 0.5)),
    ("anthropic", "claude-opus-4-8", claude(5.0, 25.0, 0.5)),
    ("anthropic", "claude-opus-4-7", claude(5.0, 25.0, 0.5)),
    ("anthropic", "claude-opus-4-6", claude(5.0, 25.0, 0.5)),
    ("anthropic", "claude-opus-4-5", claude(5.0, 25.0, 0.5)),
    ("anthropic", "claude-opus-4", claude(15.0, 75.0, 1.5)),
    ("anthropic", "claude-sonnet-5-5", claude(2.0, 10.0, 0.1)),
    ("anthropic", "claude-sonnet-5", claude(2.0, 10.0, 0.2)),
    ("anthropic", "claude-sonnet-4", claude(3.0, 15.0, 0.3)),
    ("anthropic", "claude-3-7-sonnet", claude(3.0, 15.0, 0.3)),
    ("anthropic", "claude-3-5-sonnet", claude(3.0, 15.0, 0.3)),
    (
        "anthropic",
        "claude-haiku-5-5",
        claude(0.1, 0.5, 0.01).long_context(100_000, 500, 500),
    ),
    ("anthropic", "claude-haiku-4-5", claude(1.0, 5.0, 0.1)),
    ("anthropic", "claude-3-5-haiku", claude(0.8, 4.0, 0.08)),
    ("anthropic", "claude-3-haiku", claude(0.25, 1.25, 0.03)),
    // Google
    ("google", "gemini-3-pro", usd(2.0, 12.0, 0.2, 2.0)),
    (
        "google",
        "gemini-2.5-pro",
        usd(1.25, 10.0, 0.125, 1.25).long_context(200_000, 200, 150),
    ),
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

/// The cached-input price of an override that leaves it out: providers with prompt caching
/// charge about a tenth of input for a cache read, so an override keeps the cache discount
/// instead of billing every read as fresh input.
fn default_cached_input(provider: &str, input: u64) -> u64 {
    match provider {
        "anthropic" | "openai" | "google" | "xai" | "deepseek" => input / 10,
        _ => input,
    }
}

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
            let cache_write = o
                .cache_write_per_mtok
                .map(usd_to_micros)
                .unwrap_or(default_write);
            // The long lifetime costs more to write: 2x input against 1.25x at Anthropic, 2.5x
            // against 1.25x at OpenAI.
            let cache_write_long = match provider.as_str() {
                "anthropic" => cache_write * 8 / 5,
                "openai" => cache_write * 2,
                _ => cache_write,
            };
            parsed.push(Entry {
                prefix: canonical_model(&o.model),
                price: Price {
                    input,
                    output: usd_to_micros(o.output_per_mtok),
                    cached_input: o
                        .cached_input_per_mtok
                        .map(usd_to_micros)
                        .unwrap_or_else(|| default_cached_input(&provider, input)),
                    cache_write,
                    cache_write_long,
                    long_context_above: 0,
                    long_input_pct: 0,
                    long_output_pct: 0,
                },
                provider,
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
        // A left-out cached price keeps the cache discount (a tenth of input).
        assert_eq!(
            (p.input, p.output, p.cached_input),
            (1_000_000, 2_000_000, 100_000)
        );
        // Providers without prompt caching bill cached input as input.
        assert_eq!(
            c.lookup_any("together", "qwen3-coder").0.cached_input,
            500_000
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
    fn newer_models_cache_lifetimes_and_long_prompts_are_priced() {
        let c = Catalog::builtin();
        let price = |p: Provider, m: &str| {
            let (price, known) = c.lookup(p, m);
            assert!(known, "{m} should be priced");
            price
        };
        // GPT-5.5 and later have their own rows instead of the most expensive fallback.
        assert_eq!(price(Provider::OpenAI, "gpt-5.5").input, 5_000_000);
        assert_eq!(
            price(Provider::OpenAI, "gpt-6.2-sol-2026-09-01").output,
            10_000_000
        );
        // Sonnet 5.5 reads at $0.10; Fable 5.2 has its own row.
        assert_eq!(
            price(Provider::Anthropic, "claude-sonnet-5-5").cached_input,
            100_000
        );
        assert_eq!(
            price(Provider::Anthropic, "claude-fable-5-2").input,
            8_000_000
        );
        // A 1-hour Anthropic write costs 2x input, a 5-minute one 1.25x.
        let opus = price(Provider::Anthropic, "claude-opus-5-5");
        let writes = |long| Usage {
            cache_write: 1_000_000,
            cache_write_long: long,
            ..Usage::default()
        };
        assert_eq!(opus.usage_cost(&writes(0)), 5_000_000);
        assert_eq!(opus.usage_cost(&writes(1_000_000)), 8_000_000);
        // OpenAI 12-hour writes cost 2.5x input.
        let sol = price(Provider::OpenAI, "gpt-6.2-sol");
        assert_eq!(
            (sol.cache_write, sol.cache_write_long),
            (2_500_000, 5_000_000)
        );
        // Haiku 5.5 prompts over 100k tokens cost 5x; cache reads count toward the length.
        let haiku = price(Provider::Anthropic, "claude-haiku-5-5");
        let short = Usage {
            input: 50_000,
            output: 1_000,
            ..Usage::default()
        };
        let long = Usage {
            input: 50_000,
            cached_input: 60_000,
            output: 1_000,
            ..Usage::default()
        };
        assert_eq!(haiku.usage_cost(&short), 5_000 + 500);
        assert_eq!(haiku.usage_cost(&long), 25_000 + 3_000 + 2_500);
        // Savings: cache reads priced as fresh input, minus what they cost.
        assert_eq!(haiku.cache_savings(&long), 30_000 - 3_000);
    }

    #[test]
    fn cost_rounds_up_to_the_micro() {
        assert_eq!(tokens_cost(1, 150_000), 1);
        assert_eq!(tokens_cost(1_000_000, 150_000), 150_000);
        assert_eq!(tokens_cost(0, 150_000), 0);
    }
}
