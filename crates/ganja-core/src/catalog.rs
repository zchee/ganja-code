//! What each model costs and how much it can hold.
//!
//! The table is a pruned snapshot of <https://models.dev/api.json> taken on
//! **2026-08-03**, covering the current generation of the two providers P2
//! ships. It is compiled in rather than fetched so that pricing works offline
//! and so that a session's cost cannot change under it mid-run; P5 adds the
//! live catalog fetch with a 24h cache and falls back to exactly this table.
//!
//! Display names are the upstream `name` field with a trailing "(latest)"
//! dropped, because a table column is not the place to explain aliasing.
//!
//! Prices are US dollars per million tokens, the unit models.dev publishes.

use crate::protocol::Usage;

/// Tokens a price is quoted per.
const PER: f64 = 1_000_000.0;

/// What a provider charges for a million tokens of each kind.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Pricing {
    /// Input tokens the provider read fresh.
    pub input: f64,
    /// Tokens the model generated, thinking included.
    pub output: f64,
    /// Input tokens served from the provider's prompt cache.
    pub cache_read: f64,
    /// Input tokens written into the prompt cache, where that is billed apart.
    ///
    /// [`None`] means the provider bills a cache write as ordinary input,
    /// which is what OpenAI-style automatic caching does; [`cost`] prices those
    /// tokens at [`Pricing::input`].
    pub cache_write: Option<f64>,
}

/// One model this build knows how to price and size.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ModelInfo {
    /// Identifier the provider expects on the wire.
    pub id: &'static str,
    /// Provider that serves it, spelled as [`Provider::id`](crate::provider::Provider::id).
    pub provider_id: &'static str,
    /// Name to show a person.
    pub name: &'static str,
    /// Tokens the model can be given, prompt and reply together.
    pub context_window: u64,
    /// Tokens it will generate in one reply before stopping.
    pub max_output: u64,
    /// What it charges.
    pub pricing: Pricing,
}

/// What one turn cost, in US dollars.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Cost {
    /// Everything charged on the way in: fresh input, cache reads, cache
    /// writes.
    pub input_usd: f64,
    /// Everything charged on the way out.
    pub output_usd: f64,
    /// The two above, which is what a status bar shows.
    pub total_usd: f64,
}

/// The model each provider is asked for when nothing says otherwise.
const DEFAULTS: &[(&str, &str)] = &[("anthropic", "claude-sonnet-5"), ("openai", "gpt-5.6")];

/// The snapshot itself.
const MODELS: &[ModelInfo] = &[
    ModelInfo {
        id: "claude-sonnet-5",
        provider_id: "anthropic",
        name: "Claude Sonnet 5",
        context_window: 1_000_000,
        max_output: 128_000,
        pricing: Pricing {
            input: 2.0,
            output: 10.0,
            cache_read: 0.2,
            cache_write: Some(2.5),
        },
    },
    ModelInfo {
        id: "claude-opus-5",
        provider_id: "anthropic",
        name: "Claude Opus 5",
        context_window: 1_000_000,
        max_output: 128_000,
        pricing: Pricing {
            input: 5.0,
            output: 25.0,
            cache_read: 0.5,
            cache_write: Some(6.25),
        },
    },
    ModelInfo {
        id: "claude-opus-4-8",
        provider_id: "anthropic",
        name: "Claude Opus 4.8",
        context_window: 1_000_000,
        max_output: 128_000,
        pricing: Pricing {
            input: 5.0,
            output: 25.0,
            cache_read: 0.5,
            cache_write: Some(6.25),
        },
    },
    ModelInfo {
        id: "claude-sonnet-4-6",
        provider_id: "anthropic",
        name: "Claude Sonnet 4.6",
        context_window: 1_000_000,
        max_output: 128_000,
        pricing: Pricing {
            input: 3.0,
            output: 15.0,
            cache_read: 0.3,
            cache_write: Some(3.75),
        },
    },
    ModelInfo {
        id: "claude-haiku-4-5",
        provider_id: "anthropic",
        name: "Claude Haiku 4.5",
        context_window: 200_000,
        max_output: 64_000,
        pricing: Pricing {
            input: 1.0,
            output: 5.0,
            cache_read: 0.1,
            cache_write: Some(1.25),
        },
    },
    ModelInfo {
        id: "gpt-5.6",
        provider_id: "openai",
        name: "GPT-5.6",
        context_window: 1_050_000,
        max_output: 128_000,
        pricing: Pricing {
            input: 5.0,
            output: 30.0,
            cache_read: 0.5,
            cache_write: Some(6.25),
        },
    },
    ModelInfo {
        id: "gpt-5.4",
        provider_id: "openai",
        name: "GPT-5.4",
        context_window: 1_050_000,
        max_output: 128_000,
        pricing: Pricing {
            input: 2.5,
            output: 15.0,
            cache_read: 0.25,
            cache_write: None,
        },
    },
    ModelInfo {
        id: "gpt-5.4-mini",
        provider_id: "openai",
        name: "GPT-5.4 mini",
        context_window: 400_000,
        max_output: 128_000,
        pricing: Pricing {
            input: 0.75,
            output: 4.5,
            cache_read: 0.075,
            cache_write: None,
        },
    },
    ModelInfo {
        id: "gpt-5.4-nano",
        provider_id: "openai",
        name: "GPT-5.4 nano",
        context_window: 400_000,
        max_output: 128_000,
        pricing: Pricing {
            input: 0.2,
            output: 1.25,
            cache_read: 0.02,
            cache_write: None,
        },
    },
    ModelInfo {
        id: "gpt-5.3-codex",
        provider_id: "openai",
        name: "GPT-5.3 Codex",
        context_window: 400_000,
        max_output: 128_000,
        pricing: Pricing {
            input: 1.75,
            output: 14.0,
            cache_read: 0.175,
            cache_write: None,
        },
    },
];

/// Looks up a model by the identifier providers use for it.
#[must_use]
pub fn model(id: &str) -> Option<&'static ModelInfo> {
    MODELS.iter().find(|model| model.id == id)
}

/// Every model in the table, grouped by provider in the order they are listed.
pub fn models() -> impl Iterator<Item = &'static ModelInfo> {
    MODELS.iter()
}

/// The model `provider_id` is asked for when the user names none.
///
/// [`None`] for a provider the table does not cover: answering with some other
/// provider's model would be a silent misconfiguration rather than a default.
#[must_use]
pub fn default_model(provider_id: &str) -> Option<&'static str> {
    DEFAULTS
        .iter()
        .find(|(provider, _)| *provider == provider_id)
        .map(|(_, model)| *model)
}

/// Renders a token count for somewhere there is no room to spell it out.
///
/// Counts below a thousand are exact, because the difference between 12 and 90
/// tokens is worth seeing; above that a tenth of the unit is as much precision
/// as a status bar or a table column can justify.
#[must_use]
pub fn compact_tokens(tokens: u64) -> String {
    const THOUSAND: f64 = 1_000.0;
    const MILLION: f64 = 1_000_000.0;

    let count = tokens as f64;
    if count < THOUSAND {
        return tokens.to_string();
    }

    // Rounding can push a count into the next unit: 999,950 tokens reads as
    // 1.0M rather than as the 1000.0k a naive division would print.
    if count < MILLION - 50.0 {
        return format!("{:.1}k", count / THOUSAND);
    }

    format!("{:.1}M", count / MILLION)
}

/// Prices one turn's [`Usage`] against `model`.
///
/// The three input counters are treated as disjoint — plain input, cache reads,
/// and cache writes each billed at their own rate — which is the shape
/// [`Usage`] documents and which providers normalize to.
/// [`Usage::reasoning_tokens`] is deliberately not priced: it counts a subset of
/// [`Usage::output_tokens`] that both providers already bill as output.
#[must_use]
pub fn cost(usage: &Usage, model: &ModelInfo) -> Cost {
    let pricing = &model.pricing;
    let priced = |per_mtok: f64, tokens: u64| per_mtok * tokens as f64 / PER;

    let input_usd = priced(pricing.input, usage.input_tokens)
        + priced(pricing.cache_read, usage.cache_read_tokens)
        + priced(
            pricing.cache_write.unwrap_or(pricing.input),
            usage.cache_write_tokens,
        );
    let output_usd = priced(pricing.output, usage.output_tokens);

    Cost {
        input_usd,
        output_usd,
        total_usd: input_usd + output_usd,
    }
}

#[cfg(test)]
mod tests {
    use super::{Cost, MODELS, compact_tokens, cost, default_model, model, models};
    use crate::protocol::Usage;

    /// Dollar amounts are compared with a tolerance because the arithmetic is
    /// binary floating point; a tenth of a cent is far below anything shown.
    fn close(left: f64, right: f64) -> bool {
        (left - right).abs() < 1e-9
    }

    #[test]
    fn every_row_is_priced_and_sized() {
        assert!(!MODELS.is_empty());

        for model in models() {
            assert!(!model.id.is_empty(), "{model:?}");
            assert!(!model.name.is_empty(), "{model:?}");
            assert!(model.context_window > 0, "{model:?}");
            assert!(
                model.max_output > 0 && model.max_output <= model.context_window,
                "a reply cannot exceed the window: {model:?}"
            );
            assert!(model.pricing.input > 0.0, "{model:?}");
            assert!(
                model.pricing.output >= model.pricing.input,
                "output has never been cheaper than input: {model:?}"
            );
            assert!(
                model.pricing.cache_read < model.pricing.input,
                "a cache read is the discount: {model:?}"
            );
        }
    }

    #[test]
    fn ids_are_unique_so_a_lookup_cannot_be_ambiguous() {
        let mut ids: Vec<&str> = models().map(|model| model.id).collect();
        let total = ids.len();
        ids.sort_unstable();
        ids.dedup();

        assert_eq!(ids.len(), total, "duplicate model id in the table");
    }

    #[test]
    fn a_known_model_resolves_and_an_unknown_one_does_not() {
        let sonnet = model("claude-sonnet-5").expect("the snapshot carries sonnet");

        assert_eq!(sonnet.provider_id, "anthropic");
        assert_eq!(sonnet.context_window, 1_000_000);
        assert!(model("claude-sonnet-3-nonexistent").is_none());
        // The fake provider's model is deliberately absent: nothing canned has
        // a price.
        assert!(model("canned").is_none());
    }

    /// Every provider a session can select has to have a default here, because
    /// this table is what resolves the model when the user names none — a
    /// provider missing from it fails at startup with `NoDefaultModel`. The
    /// list is derived from `provider::PROVIDERS` rather than written out
    /// again: two hand-maintained lists in different modules is precisely how
    /// a provider gets added on one side and forgotten on the other.
    #[test]
    fn every_selectable_provider_has_a_default_this_table_can_price() {
        for provider in crate::provider::PROVIDERS {
            // The fake provider carries its own canned model and is
            // deliberately unpriced; nothing about it is billable.
            if provider == crate::provider::fake::ID {
                continue;
            }

            let id = default_model(provider)
                .unwrap_or_else(|| panic!("{provider} is selectable but has no default model"));
            let info = model(id)
                .unwrap_or_else(|| panic!("{provider}'s default {id} is not in the table"));

            assert_eq!(info.provider_id, provider, "{id} is not {provider}'s");
        }

        assert!(default_model("nonexistent").is_none());
    }

    #[test]
    fn a_turn_with_cache_traffic_prices_every_counter() {
        let sonnet = model("claude-sonnet-5").expect("the snapshot carries sonnet");
        let usage = Usage {
            input_tokens: 1_000_000,
            output_tokens: 1_000_000,
            reasoning_tokens: 500_000,
            cache_read_tokens: 2_000_000,
            cache_write_tokens: 1_000_000,
        };

        let Cost {
            input_usd,
            output_usd,
            total_usd,
        } = cost(&usage, sonnet);

        // 1 MTok fresh at $2 + 2 MTok cached at $0.20 + 1 MTok written at $2.50.
        assert!(close(input_usd, 2.0 + 0.4 + 2.5), "got {input_usd}");
        // Output is $10/MTok and the reasoning tokens are part of it, not extra.
        assert!(close(output_usd, 10.0), "got {output_usd}");
        assert!(close(total_usd, input_usd + output_usd));
    }

    /// A provider that does not bill cache writes apart still has them priced,
    /// at the plain input rate, rather than silently free.
    #[test]
    fn a_cache_write_without_its_own_price_bills_as_input() {
        let nano = model("gpt-5.4-nano").expect("the snapshot carries nano");
        assert!(nano.pricing.cache_write.is_none());

        let usage = Usage {
            cache_write_tokens: 1_000_000,
            ..Usage::default()
        };

        assert!(close(cost(&usage, nano).input_usd, nano.pricing.input));
    }

    #[test]
    fn a_token_count_stays_readable_at_every_magnitude() {
        let cases = [
            (0, "0"),
            (7, "7"),
            (999, "999"),
            (1_000, "1.0k"),
            (12_345, "12.3k"),
            (999_949, "999.9k"),
            // The boundary rounding would otherwise print as "1000.0k".
            (999_950, "1.0M"),
            (1_000_000, "1.0M"),
            (1_050_000, "1.1M"),
        ];

        for (tokens, expected) in cases {
            assert_eq!(compact_tokens(tokens), expected, "for {tokens}");
        }
    }

    #[test]
    fn an_empty_turn_costs_nothing() {
        let sonnet = model("claude-sonnet-5").expect("the snapshot carries sonnet");

        assert_eq!(cost(&Usage::default(), sonnet), Cost::default());
    }

    /// Rounding to the four decimals the status bar shows must not swallow a
    /// short turn: a thousand-token exchange still registers.
    #[test]
    fn a_short_turn_is_still_worth_a_visible_amount() {
        let opus = model("claude-opus-5").expect("the snapshot carries opus");
        let usage = Usage {
            input_tokens: 12_000,
            output_tokens: 800,
            ..Usage::default()
        };

        let total = cost(&usage, opus).total_usd;

        assert!(close(total, 0.06 + 0.02), "got {total}");
    }
}
