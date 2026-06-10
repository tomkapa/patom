//! Domain newtypes for the spend budget (CLAUDE.md §1). Every value that
//! carries an invariant is wrapped; the `TryFrom` impl is the only way in.

use chrono::{Datelike, NaiveDate};

use crate::clock::SharedClock;
use crate::types::ParseError;

/// Money in micro-USD (`1e-6` USD). A single turn's cost is always `>= 0`;
/// the period total is a `BIGINT` counter that never approaches `i64::MAX`
/// for any realistic spend.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct CostMicros(i64);

impl CostMicros {
    /// A free (zero-cost) turn — e.g. a provider that reports no usage.
    pub const ZERO: Self = Self(0);

    #[must_use]
    pub const fn get(self) -> i64 {
        self.0
    }
}

impl TryFrom<i64> for CostMicros {
    type Error = ParseError;
    fn try_from(raw: i64) -> Result<Self, Self::Error> {
        if raw < 0 {
            return Err(ParseError::OutOfRange {
                field: "cost_micros",
                detail: "must be >= 0",
            });
        }
        Ok(Self(raw))
    }
}

impl TryFrom<i128> for CostMicros {
    type Error = ParseError;
    fn try_from(raw: i128) -> Result<Self, Self::Error> {
        if raw < 0 {
            return Err(ParseError::OutOfRange {
                field: "cost_micros",
                detail: "must be >= 0",
            });
        }
        let v = i64::try_from(raw).map_err(|_| ParseError::OutOfRange {
            field: "cost_micros",
            detail: "exceeds i64",
        })?;
        Ok(Self(v))
    }
}

/// An org's configured monthly spend cap in micro-USD.
///
/// The `org_billing` column is `BIGINT CHECK (... > 0)` — the absence of a cap
/// (unlimited) is modelled as `Option::None` at the boundary, never a stored
/// zero, so a `MonthlyCapMicros` is always strictly positive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct MonthlyCapMicros(i64);

impl MonthlyCapMicros {
    #[must_use]
    pub const fn get(self) -> i64 {
        self.0
    }
}

impl TryFrom<i64> for MonthlyCapMicros {
    type Error = ParseError;
    fn try_from(raw: i64) -> Result<Self, Self::Error> {
        if raw <= 0 {
            return Err(ParseError::OutOfRange {
                field: "monthly_cap_micro_usd",
                detail: "must be > 0",
            });
        }
        Ok(Self(raw))
    }
}

/// A token price as micro-USD per **million** tokens.
///
/// This is the conventional vendor pricing unit. Keeping the rate per-million
/// lets the cost calculator stay in integers: `cost = tokens * rate / 1_000_000`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct MicroUsdPerMtok(i64);

impl MicroUsdPerMtok {
    /// Construct a rate. `const` so the static price table is a compile-time
    /// literal; non-negativity is asserted by a `#[test]` over the catalog.
    #[must_use]
    pub const fn new(micro_usd_per_mtok: i64) -> Self {
        Self(micro_usd_per_mtok)
    }

    #[must_use]
    pub const fn get(self) -> i64 {
        self.0
    }
}

/// Per-`(model, provider)` price across the four token lanes a provider can
/// bill. Cache lanes are zero for providers that don't report caching.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Price {
    pub input: MicroUsdPerMtok,
    pub output: MicroUsdPerMtok,
    pub cache_write: MicroUsdPerMtok,
    pub cache_read: MicroUsdPerMtok,
}

impl Price {
    /// Convenience literal constructor for the static table (`limits` /
    /// `pricing`).
    #[must_use]
    pub const fn new(input: i64, output: i64, cache_write: i64, cache_read: i64) -> Self {
        Self {
            input: MicroUsdPerMtok::new(input),
            output: MicroUsdPerMtok::new(output),
            cache_write: MicroUsdPerMtok::new(cache_write),
            cache_read: MicroUsdPerMtok::new(cache_read),
        }
    }
}

/// Soft-alert threshold in basis points (8000 = 80%). `1..=10000`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct WarnThresholdBps(u16);

impl WarnThresholdBps {
    /// Total basis points in 100% — the denominator for threshold math.
    pub const FULL: u16 = 10_000;

    #[must_use]
    pub const fn get(self) -> u16 {
        self.0
    }
}

impl TryFrom<u16> for WarnThresholdBps {
    type Error = ParseError;
    fn try_from(raw: u16) -> Result<Self, Self::Error> {
        if raw == 0 {
            return Err(ParseError::OutOfRange {
                field: "warn_threshold_bps",
                detail: "must be > 0",
            });
        }
        if raw > Self::FULL {
            return Err(ParseError::OutOfRange {
                field: "warn_threshold_bps",
                detail: "must be <= 10000",
            });
        }
        Ok(Self(raw))
    }
}

/// First day of a billing month (UTC). The `org_billing_usage` primary key is
/// `(org_id, period_start)`, so a new month is a fresh row whose counter
/// starts at zero — that *is* the monthly reset.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct BillingPeriod(NaiveDate);

impl BillingPeriod {
    /// The period containing `clock`'s current instant. Derived from the
    /// injected [`Clock`](crate::clock::Clock) (CLAUDE.md §11) — never
    /// `Utc::now` — so tests pin the boundary with a `TestClock`.
    #[must_use]
    pub fn current(clock: &SharedClock) -> Self {
        let today = clock.now_utc().date_naive();
        // §6: every calendar month has a day 1; `with_day(1)` cannot fail.
        let first = today
            .with_day(1)
            .expect("invariant: day 1 is valid for every month");
        Self(first)
    }

    /// The stored `DATE` value (first-of-month).
    #[must_use]
    pub const fn start_date(self) -> NaiveDate {
        self.0
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use chrono::Datelike;

    use crate::clock::TestClock;

    use super::*;

    #[test]
    fn cost_micros_rejects_negative() {
        assert!(CostMicros::try_from(-1_i64).is_err());
        assert!(CostMicros::try_from(-1_i128).is_err());
        assert_eq!(CostMicros::try_from(0_i64).expect("valid").get(), 0);
        assert_eq!(CostMicros::try_from(42_i64).expect("valid").get(), 42);
    }

    #[test]
    fn cost_micros_i128_rejects_overflow() {
        let too_big = i128::from(i64::MAX) + 1;
        assert!(CostMicros::try_from(too_big).is_err());
        assert_eq!(
            CostMicros::try_from(i128::from(i64::MAX))
                .expect("fits")
                .get(),
            i64::MAX
        );
    }

    #[test]
    fn monthly_cap_rejects_non_positive() {
        assert!(MonthlyCapMicros::try_from(0_i64).is_err());
        assert!(MonthlyCapMicros::try_from(-1_i64).is_err());
        assert_eq!(
            MonthlyCapMicros::try_from(5_000_000_i64)
                .expect("valid")
                .get(),
            5_000_000
        );
    }

    #[test]
    fn warn_threshold_bounds() {
        assert!(WarnThresholdBps::try_from(0).is_err());
        assert!(WarnThresholdBps::try_from(WarnThresholdBps::FULL + 1).is_err());
        assert_eq!(WarnThresholdBps::try_from(8000).expect("valid").get(), 8000);
        assert_eq!(
            WarnThresholdBps::try_from(WarnThresholdBps::FULL)
                .expect("valid")
                .get(),
            10_000
        );
    }

    #[test]
    fn billing_period_is_first_of_month() {
        // TestClock starts "now"; whatever month it lands in, the period is day 1.
        let clock: SharedClock = Arc::new(TestClock::new());
        let period = BillingPeriod::current(&clock);
        assert_eq!(period.start_date().day(), 1);
    }

    #[test]
    // from_hours/from_days (the lint's suggestion) are unstable on this
    // toolchain; from_mins is the largest stable unit available.
    #[allow(clippy::duration_suboptimal_units)]
    fn billing_period_flips_across_month_boundary() {
        let test_clock = Arc::new(TestClock::new());
        let clock: SharedClock = test_clock.clone();
        let start = BillingPeriod::current(&clock);
        // Advance ~70 days — guaranteed to cross at least one month boundary
        // regardless of which day the test runs.
        test_clock.advance(Duration::from_mins(100_800)); // 70 days
        let later = BillingPeriod::current(&clock);
        assert_ne!(start.start_date(), later.start_date());
        assert!(later.start_date() > start.start_date());
        assert_eq!(later.start_date().day(), 1);
    }
}
