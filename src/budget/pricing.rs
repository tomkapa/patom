//! Static token→cost price table and the per-turn cost calculator.
//!
//! Prices are hardcoded per `(model)` — the catalog name is unique, so it keys
//! the table directly. The table is intentionally in-tree (CLAUDE.md §8/§9): a
//! price change is a deploy, which is also the audit trail. An unknown name
//! falls back to the pessimistic [`limits::FALLBACK_PRICE`] and emits a WARN so
//! ops can add the missing entry — spend is never silently free (§5).
//!
//! All arithmetic is integer (CLAUDE.md §7: `float_cmp`/`as_conversions`
//! denied). A rate is micro-USD per million tokens, so
//! `cost = tokens * rate / 1_000_000`, computed in `i128` to keep the multiply
//! from overflowing before the divide.

use tracing::warn;

use crate::provider::{Model, Usage};

use super::limits::FALLBACK_PRICE;
use super::types::{CostMicros, Price};

/// Price for a catalog model. Pure getter — see [`price_for_name`] for the
/// table and the unknown-name fallback.
#[must_use]
pub fn price_for(model: Model) -> Price {
    price_for_name(model.as_str())
}

/// Table body, split out so the fallback branch is reachable from unit tests
/// with an arbitrary string (a real [`Model`] can only ever be a catalog name).
fn price_for_name(name: &str) -> Price {
    // micro-USD per million tokens: input, output, cache_write, cache_read.
    // Vendor list prices as of the catalog generation (May 2026); see
    // src/provider/catalog.rs for the source links.
    match name {
        // Anthropic. Cache-write = 1.25× input, cache-read = 0.1× input.
        "claude-opus-4-7" => Price::new(15_000_000, 75_000_000, 18_750_000, 1_500_000),
        "claude-sonnet-4-6" | "claude-sonnet-4-5" => {
            Price::new(3_000_000, 15_000_000, 3_750_000, 300_000)
        }
        "claude-haiku-4-5" => Price::new(800_000, 4_000_000, 1_000_000, 80_000),
        // OpenAI. No cache-write surcharge (cache_write = input);
        // cache-read ≈ 0.1× input.
        "gpt-5.5" => Price::new(2_500_000, 20_000_000, 2_500_000, 250_000),
        "gpt-5.4" => Price::new(1_250_000, 10_000_000, 1_250_000, 125_000),
        "gpt-5.4-mini" => Price::new(250_000, 2_000_000, 250_000, 25_000),
        "gpt-5.4-nano" => Price::new(50_000, 400_000, 50_000, 5_000),
        "gpt-4o-mini" => Price::new(150_000, 600_000, 150_000, 15_000),
        // DeepSeek.
        "deepseek-v4-pro" => Price::new(270_000, 1_100_000, 270_000, 27_000),
        "deepseek-v4-flash" => Price::new(70_000, 280_000, 70_000, 7_000),
        // Test-only sentinels, present only under the `test-catalog` feature
        // (see catalog::TEST_CATALOG_EXTENSION). Priced so the completeness
        // test holds without exposing them to release builds.
        #[cfg(feature = "test-catalog")]
        "test-model" => Price::new(3_000_000, 15_000_000, 3_750_000, 300_000),
        #[cfg(feature = "test-catalog")]
        "test-model-openai" => Price::new(1_250_000, 10_000_000, 1_250_000, 125_000),
        _ => {
            warn!(
                event = "budget.price.fallback",
                patom.model = name,
                "no price entry; using pessimistic fallback"
            );
            FALLBACK_PRICE
        }
    }
}

/// Cost of one turn given its price and the provider's reported [`Usage`].
/// Cache lanes default to `0` when the provider doesn't report caching.
///
/// # Panics
/// Panics (a named assertion, CLAUDE.md §6) if the total overflows `i64` —
/// impossible for `u32`-bounded token counts, so observing it means a caller
/// fabricated impossible usage.
#[must_use]
pub fn turn_cost(price: Price, usage: &Usage) -> CostMicros {
    let lanes = [
        (i64::from(usage.input_tokens), price.input),
        (i64::from(usage.output_tokens), price.output),
        (
            i64::from(usage.cache_creation_input_tokens.unwrap_or(0)),
            price.cache_write,
        ),
        (
            i64::from(usage.cache_read_input_tokens.unwrap_or(0)),
            price.cache_read,
        ),
    ];
    let mut total_micros: i128 = 0;
    for (tokens, rate) in lanes {
        // Counts come from `u32`, so each lane is non-negative by construction;
        // i128 multiply can't overflow for those bounds. Divide last to keep
        // integer precision.
        total_micros += i128::from(tokens) * i128::from(rate.get()) / 1_000_000;
    }
    assert!(total_micros >= 0, "invariant: total cost non-negative");
    CostMicros::try_from(total_micros).expect("invariant: bounded turn cost fits i64 micro-USD")
}

#[cfg(test)]
mod tests {
    use crate::provider::Model;

    use super::super::limits::FALLBACK_PRICE;
    use super::*;

    #[test]
    fn every_catalog_model_has_a_real_price() {
        // Completeness (CLAUDE.md §5): adding a catalog model without a price
        // fails the suite rather than silently falling back. FALLBACK_PRICE is
        // strictly more expensive than any real entry, so no real model equals
        // it — equality here means a missing arm.
        for model in Model::all() {
            assert_ne!(
                price_for(model),
                FALLBACK_PRICE,
                "model {} has no price entry",
                model.as_str()
            );
        }
    }

    #[test]
    fn unknown_name_falls_back() {
        assert_eq!(price_for_name("totally-made-up-model"), FALLBACK_PRICE);
    }

    #[test]
    fn fallback_dominates_every_catalog_price() {
        // The fallback must be a pessimistic ceiling on every lane, or an
        // unpriced model could be billed *less* than a real one.
        for model in Model::all() {
            let p = price_for(model);
            assert!(p.input.get() <= FALLBACK_PRICE.input.get());
            assert!(p.output.get() <= FALLBACK_PRICE.output.get());
            assert!(p.cache_write.get() <= FALLBACK_PRICE.cache_write.get());
            assert!(p.cache_read.get() <= FALLBACK_PRICE.cache_read.get());
        }
    }

    /// Build a `Usage` from the four lane counts (cache lanes as `Some` only
    /// when non-zero, mirroring how providers report them).
    fn usage(input: u32, output: u32, cache_write: u32, cache_read: u32) -> crate::provider::Usage {
        crate::provider::Usage {
            input_tokens: input,
            output_tokens: output,
            cache_creation_input_tokens: (cache_write != 0).then_some(cache_write),
            cache_read_input_tokens: (cache_read != 0).then_some(cache_read),
        }
    }

    #[test]
    fn turn_cost_zero_tokens_is_zero() {
        let price = price_for_name("claude-sonnet-4-6");
        assert_eq!(turn_cost(price, &usage(0, 0, 0, 0)), CostMicros::ZERO);
    }

    #[test]
    fn turn_cost_exact_integer_math() {
        // Sonnet: input $3/Mtok, output $15/Mtok. 1M input + 1M output =
        // $3 + $15 = $18 = 18_000_000 micro-USD. Exact — no float.
        let price = price_for_name("claude-sonnet-4-6");
        let cost = turn_cost(price, &usage(1_000_000, 1_000_000, 0, 0));
        assert_eq!(cost.get(), 18_000_000);
    }

    #[test]
    fn turn_cost_sums_all_four_lanes() {
        // 1M tokens on each lane sums the four per-Mtok rates directly.
        let price = price_for_name("claude-opus-4-7");
        let cost = turn_cost(price, &usage(1_000_000, 1_000_000, 1_000_000, 1_000_000));
        assert_eq!(cost.get(), 15_000_000 + 75_000_000 + 18_750_000 + 1_500_000);
    }

    #[test]
    fn turn_cost_sub_million_truncates_toward_zero() {
        // 500k input at $3/Mtok = $1.50 = 1_500_000 micro-USD, exactly.
        let price = price_for_name("claude-sonnet-4-6");
        assert_eq!(turn_cost(price, &usage(500_000, 0, 0, 0)).get(), 1_500_000);
        // 1 token input at $3/Mtok = 3_000_000 micro/Mtok * 1 / 1e6 = 3 micro-USD.
        assert_eq!(turn_cost(price, &usage(1, 0, 0, 0)).get(), 3);
        // cache_read rate is 300_000 micro/Mtok: 3 tokens → 900_000/1e6 = 0
        // (truncates), 4 tokens → 1_200_000/1e6 = 1.
        assert_eq!(turn_cost(price, &usage(0, 0, 0, 3)).get(), 0);
        assert_eq!(turn_cost(price, &usage(0, 0, 0, 4)).get(), 1);
    }
}
