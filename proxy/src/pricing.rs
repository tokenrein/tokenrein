use anyhow::{Result, bail};

use crate::config::{PriceOverride, usd_to_micros};
use crate::providers::{Provider, Usage};

/// Prices in micro-USD per million tokens.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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

/// Month the built-in list prices were last checked; override in config (`[[pricing]]`) when they change.
pub const BUILTIN_VERIFIED_AS_OF: &str = "2025-11";

const fn price(input: u64, output: u64, cached_input: u64, cache_write: u64) -> Price {
    Price {
        input,
        output,
        cached_input,
        cache_write,
    }
}

const BUILTIN: &[(Provider, &str, Price)] = &[
    (
        Provider::OpenAI,
        "gpt-4o",
        price(2_500_000, 10_000_000, 1_250_000, 0),
    ),
    (
        Provider::OpenAI,
        "gpt-4o-mini",
        price(150_000, 600_000, 75_000, 0),
    ),
    (
        Provider::OpenAI,
        "gpt-4.1",
        price(2_000_000, 8_000_000, 500_000, 0),
    ),
    (
        Provider::OpenAI,
        "gpt-4.1-mini",
        price(400_000, 1_600_000, 100_000, 0),
    ),
    (
        Provider::OpenAI,
        "gpt-4.1-nano",
        price(100_000, 400_000, 25_000, 0),
    ),
    (
        Provider::OpenAI,
        "gpt-5",
        price(1_250_000, 10_000_000, 125_000, 0),
    ),
    (
        Provider::OpenAI,
        "gpt-5-mini",
        price(250_000, 2_000_000, 25_000, 0),
    ),
    (
        Provider::OpenAI,
        "gpt-5-nano",
        price(50_000, 400_000, 5_000, 0),
    ),
    (
        Provider::OpenAI,
        "o1",
        price(15_000_000, 60_000_000, 7_500_000, 0),
    ),
    (
        Provider::OpenAI,
        "o1-mini",
        price(1_100_000, 4_400_000, 550_000, 0),
    ),
    (
        Provider::OpenAI,
        "o3",
        price(2_000_000, 8_000_000, 500_000, 0),
    ),
    (
        Provider::OpenAI,
        "o3-mini",
        price(1_100_000, 4_400_000, 550_000, 0),
    ),
    (
        Provider::OpenAI,
        "o4-mini",
        price(1_100_000, 4_400_000, 275_000, 0),
    ),
    (
        Provider::Anthropic,
        "claude-opus-4",
        price(15_000_000, 75_000_000, 1_500_000, 18_750_000),
    ),
    (
        Provider::Anthropic,
        "claude-opus-4-5",
        price(5_000_000, 25_000_000, 500_000, 6_250_000),
    ),
    (
        Provider::Anthropic,
        "claude-sonnet-4",
        price(3_000_000, 15_000_000, 300_000, 3_750_000),
    ),
    (
        Provider::Anthropic,
        "claude-3-7-sonnet",
        price(3_000_000, 15_000_000, 300_000, 3_750_000),
    ),
    (
        Provider::Anthropic,
        "claude-3-5-sonnet",
        price(3_000_000, 15_000_000, 300_000, 3_750_000),
    ),
    (
        Provider::Anthropic,
        "claude-haiku-4-5",
        price(1_000_000, 5_000_000, 100_000, 1_250_000),
    ),
    (
        Provider::Anthropic,
        "claude-3-5-haiku",
        price(800_000, 4_000_000, 80_000, 1_000_000),
    ),
    (
        Provider::Anthropic,
        "claude-3-haiku",
        price(250_000, 1_250_000, 30_000, 300_000),
    ),
];

type Entry = (Provider, String, Price);

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
            let Some(provider) = Provider::from_name(&o.provider) else {
                bail!(
                    "pricing override for {:?}: unknown provider {:?}",
                    o.model,
                    o.provider
                );
            };
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
            let default_write = match provider {
                Provider::Anthropic => input + input / 4,
                Provider::OpenAI => input,
            };
            parsed.push((
                provider,
                o.model.to_ascii_lowercase(),
                Price {
                    input,
                    output: usd_to_micros(o.output_per_mtok),
                    cached_input: o.cached_input_per_mtok.map(usd_to_micros).unwrap_or(input),
                    cache_write: o
                        .cache_write_per_mtok
                        .map(usd_to_micros)
                        .unwrap_or(default_write),
                },
            ));
        }
        Ok(Self {
            overrides: parsed,
            builtin: BUILTIN
                .iter()
                .map(|(p, m, price)| (*p, m.to_string(), *price))
                .collect(),
        })
    }

    /// Returns the price and whether the model was known; unknown models get the provider's most expensive price.
    pub fn lookup(&self, provider: Provider, model: &str) -> (Price, bool) {
        let model = model.to_ascii_lowercase();
        if let Some(p) = best_match(&self.overrides, provider, &model) {
            return (p, true);
        }
        if let Some(p) = best_match(&self.builtin, provider, &model) {
            return (p, true);
        }
        (self.fallback(provider), false)
    }

    fn fallback(&self, provider: Provider) -> Price {
        self.builtin
            .iter()
            .chain(&self.overrides)
            .filter(|(p, _, _)| *p == provider)
            .map(|(_, _, price)| *price)
            .max_by_key(|price| price.output)
            .unwrap_or(price(15_000_000, 75_000_000, 15_000_000, 18_750_000))
    }
}

fn best_match(entries: &[Entry], provider: Provider, model: &str) -> Option<Price> {
    entries
        .iter()
        .filter(|(p, prefix, _)| *p == provider && matches_prefix(model, prefix))
        .max_by_key(|(_, prefix, _)| prefix.len())
        .map(|(_, _, price)| *price)
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
        assert_eq!(
            c.lookup(Provider::OpenAI, "gpt-4o-2024-08-06").0.input,
            2_500_000
        );
        assert_eq!(
            c.lookup(Provider::Anthropic, "claude-opus-4-5-20251101")
                .0
                .output,
            25_000_000
        );
        assert_eq!(
            c.lookup(Provider::Anthropic, "claude-opus-4-1-20250805")
                .0
                .output,
            75_000_000
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
    }

    #[test]
    fn overrides_take_precedence() {
        let c = Catalog::with_overrides(&[PriceOverride {
            provider: "openai".into(),
            model: "gpt-4o".into(),
            input_per_mtok: 1.0,
            output_per_mtok: 2.0,
            cached_input_per_mtok: None,
            cache_write_per_mtok: None,
        }])
        .unwrap();
        let (p, known) = c.lookup(Provider::OpenAI, "gpt-4o");
        assert!(known);
        assert_eq!(
            (p.input, p.output, p.cached_input),
            (1_000_000, 2_000_000, 1_000_000)
        );
    }

    #[test]
    fn cost_rounds_up_to_the_micro() {
        assert_eq!(tokens_cost(1, 150_000), 1);
        assert_eq!(tokens_cost(1_000_000, 150_000), 150_000);
        assert_eq!(tokens_cost(0, 150_000), 0);
    }
}
