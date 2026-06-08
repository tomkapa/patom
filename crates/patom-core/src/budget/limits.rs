//! Budget invariants (CLAUDE.md §5): every limit named, doc-commented with
//! *why this number*, and exported so an operator can audit them in one place.

use super::types::Price;

/// Default soft-alert threshold in basis points (8000 = 80%).
///
/// Mirrors the `org_budgets.warn_threshold_bps` column default; used when a row
/// predates a configured value. 80% is the conventional "heads-up before you
/// hit the wall" point.
pub const DEFAULT_WARN_BPS: u16 = 8000;

/// Default monthly spend cap applied to every newly-created *cloud* org, in
/// micro-USD (issue #121).
///
/// Free public beta: Patom pays the provider bill, so an org with no cap is
/// open-ended liability — one bad actor, a scripted signup loop, or a launch
/// spike can drain the provider budget. $1.00 is deliberately tight: enough to
/// *try* the product, bounded enough that N scripted signups cost $N rather
/// than N×∞. Owners/admins raise it from Settings → Billing. The self-host
/// bootstrap org is exempt (the operator pays their own bill) — only the cloud
/// self-service path stamps this. Mirrored by the migration-62 backfill SQL;
/// keep the two in sync. Tunable here.
pub const DEFAULT_ORG_MONTHLY_CAP_MICROS: i64 = 1_000_000; // $1.00

/// Pessimistic price for a `(model, provider)` pair with no catalog entry
/// (CLAUDE.md §5: *unknown bound → pick a pessimistic one and add a metric*).
///
/// Every lane is `>=` the most expensive catalog model's corresponding lane
/// (today the Anthropic Opus tier: input 15 / output 75 / cache-write 18.75 /
/// cache-read 1.5 USD per Mtok). Over-billing an unpriced model is the safe
/// failure: it can only *tighten* the budget, never silently let spend run
/// free. A `pricing.price_for` fallback also emits `patom.budget.price.fallback`
/// so ops can add the missing entry. Units: micro-USD per million tokens.
pub const FALLBACK_PRICE: Price = Price::new(
    20_000_000, // input:       $20 / Mtok
    80_000_000, // output:      $80 / Mtok
    25_000_000, // cache_write: $25 / Mtok
    2_000_000,  // cache_read:   $2 / Mtok
);

/// Conservative ceiling on a single turn's cost, in micro-USD — the `T_max` in
/// the post-paid overrun bound (see module docs / plan).
///
/// Derivation: the most expensive a turn can be is a full context window of
/// input plus a maxed-out completion, priced at [`FALLBACK_PRICE`]:
/// `~1.2M input tokens * $20/Mtok ≈ $24` + `MAX_OUTPUT_TOKENS_CAP` (200k)
/// `* $80/Mtok = $16`, plus cache lanes — comfortably under $100. Worst-case
/// budget overshoot is `C * MAX_SINGLE_TURN_COST_MICROS` where `C` is the
/// number of turns running concurrently for one org (bounded by the worker
/// pool). Used only for documentation/asserts, not on the hot path.
pub const MAX_SINGLE_TURN_COST_MICROS: i64 = 100_000_000; // $100

// §5: the default threshold must parse cleanly through its newtype. Pinned at
// compile time so a future bump cannot silently fall out of range.
const _: () = assert!(DEFAULT_WARN_BPS > 0);
const _: () = assert!(DEFAULT_WARN_BPS <= 10_000);
const _: () = assert!(MAX_SINGLE_TURN_COST_MICROS > 0);
// §5: the default org cap must satisfy the `org_budgets` column CHECK (> 0) so
// a freshly-stamped row can never be rejected by the database.
const _: () = assert!(DEFAULT_ORG_MONTHLY_CAP_MICROS > 0);
