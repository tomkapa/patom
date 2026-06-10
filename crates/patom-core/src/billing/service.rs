//! Budget gate + settle — trait surface and Postgres impl.
//!
//! Mirrors [`crate::runtime::dag::DagBudget`]: an atomic counter in Postgres
//! with a privileged (worker-side) and a tenant-scoped (RLS) entry point. The
//! difference is post-paid accounting — the gate ([`BillingService::check_or_fail`])
//! reads a stale total before a turn runs, and [`BillingService::settle`] adds
//! the real cost atomically afterwards.

use std::fmt;
use std::sync::Arc;

use async_trait::async_trait;
use sqlx::{PgPool, Postgres, Transaction};
use tracing::warn;

use chrono::{DateTime, NaiveDate, Utc};

use crate::auth::{OrgId, UserId, begin_as_user, begin_privileged};
use crate::clock::SharedClock;

use super::error::BillingError;
use super::limits::DEFAULT_WARN_BPS;
use super::types::{BillingPeriod, CostMicros, MonthlyCapMicros, WarnThresholdBps};

/// Atomically add `cost` to the current period and read back the new total
/// alongside the org's config in one round-trip. A `LEFT JOIN` on `org_billing`
/// so an unconfigured (unlimited) org still returns the upserted usage with NULL
/// cap/threshold.
const SETTLE: &str = "
    WITH up AS (
        INSERT INTO org_billing_usage (org_id, period_start, used_micro_usd, created_at, updated_at)
        VALUES ($1, $2, $3, $4, $4)
        ON CONFLICT (org_id, period_start)
        DO UPDATE SET used_micro_usd = org_billing_usage.used_micro_usd + EXCLUDED.used_micro_usd,
                      updated_at     = EXCLUDED.updated_at
        RETURNING used_micro_usd
    )
    SELECT up.used_micro_usd, b.monthly_cap_micro_usd, b.warn_threshold_bps
    FROM up LEFT JOIN org_billing b ON b.org_id = $1";

/// Cap + current-period usage for the gate, in one round-trip. Driven from
/// `org_billing`: an org with no row is unlimited, so the absent-row case (no
/// cap) is exactly the pass case and the usage value is irrelevant there.
const GATE_SNAPSHOT: &str = "
    SELECT b.monthly_cap_micro_usd, u.used_micro_usd
    FROM org_billing b
    LEFT JOIN org_billing_usage u ON u.org_id = b.org_id AND u.period_start = $2
    WHERE b.org_id = $1";

/// Set `warned_at` exactly once per period, the first time usage crosses the
/// threshold. The `warned_at IS NULL` guard makes concurrent settles race-safe.
const MARK_WARNED: &str = "
    UPDATE org_billing_usage SET warned_at = $3, updated_at = $3
    WHERE org_id = $1 AND period_start = $2 AND warned_at IS NULL";

/// Read the org's config (cap + warn threshold) alongside the current period's
/// usage in one round-trip for the admin GET. Driven from a synthetic single
/// row so the result is always present: an absent `org_billing` row reads as
/// unlimited (NULL cap), an absent usage row as zero spent — and under RLS a
/// cross-tenant org argument leaves both joins NULL rather than reading another
/// org's row.
const READ_CONFIG: &str = "
    SELECT b.monthly_cap_micro_usd, b.warn_threshold_bps, u.used_micro_usd, u.warned_at
    FROM (SELECT $1::uuid AS org_id) k
    LEFT JOIN org_billing b ON b.org_id = k.org_id
    LEFT JOIN org_billing_usage u ON u.org_id = k.org_id AND u.period_start = $2";

/// Set (or clear) the cap + warn threshold for one org. `$2` NULL clears the
/// cap (unlimited). Mirrors the `set_billing` upsert in tests/common/pg.rs.
const WRITE_CONFIG: &str = "
    INSERT INTO org_billing (org_id, monthly_cap_micro_usd, warn_threshold_bps, created_at, updated_at)
    VALUES ($1, $2, $3, $4, $4)
    ON CONFLICT (org_id) DO UPDATE
        SET monthly_cap_micro_usd = EXCLUDED.monthly_cap_micro_usd,
            warn_threshold_bps    = EXCLUDED.warn_threshold_bps,
            updated_at            = EXCLUDED.updated_at";

/// One org's budget configuration plus its current-period spend.
///
/// For the admin read API. `cap_micro_usd == None` is the unlimited case;
/// `warn_threshold_bps` falls back to [`DEFAULT_WARN_BPS`] when no config row
/// exists yet.
#[derive(Debug, Clone, Copy)]
pub struct BillingConfig {
    pub cap_micro_usd: Option<i64>,
    pub warn_threshold_bps: u16,
    pub used_micro_usd: i64,
    pub warned_at: Option<DateTime<Utc>>,
    pub period_start: NaiveDate,
}

/// Operations the admission gate, per-turn gate, and settle path need.
#[async_trait]
pub trait BillingService: fmt::Debug + Send + Sync {
    /// Privileged (worker-side) gate. `Ok(())` while the org's current-period
    /// spend is under its cap, or when no cap is configured (unlimited);
    /// [`BillingError::Exceeded`] at/over the cap.
    async fn check_or_fail(&self, org: OrgId) -> Result<(), BillingError>;

    /// Tenant-scoped gate for the HTTP admission path. Opens `begin_as_user` so
    /// the read is RLS-filtered to the acting principal's org — a cross-tenant
    /// org argument reads as unlimited rather than touching another org's row.
    async fn check_or_fail_for_user(
        &self,
        acting_user_id: UserId,
        org: OrgId,
    ) -> Result<(), BillingError>;

    /// Post-paid settle (privileged, worker-side). Adds `cost` to the current
    /// period atomically and fires the soft-warn alert once per period.
    async fn settle(&self, org: OrgId, cost: CostMicros) -> Result<(), BillingError>;

    /// Tenant-scoped read for the admin GET. Opens `begin_as_user` so the read
    /// is RLS-filtered to the acting principal's org. Returns the configured cap
    /// (`None` = unlimited), warn threshold, and the current period's usage.
    async fn get_config(
        &self,
        acting_user_id: UserId,
        org: OrgId,
    ) -> Result<BillingConfig, BillingError>;

    /// Tenant-scoped write for the admin PUT. Sets/clears the cap and warn
    /// threshold via `begin_as_user` (RLS) and returns the fresh config read in
    /// the same transaction, so the caller needn't re-read. The owner/admin role
    /// gate is the caller's responsibility (enforced at the HTTP boundary).
    async fn set_config(
        &self,
        acting_user_id: UserId,
        org: OrgId,
        cap: Option<MonthlyCapMicros>,
        warn: WarnThresholdBps,
    ) -> Result<BillingConfig, BillingError>;
}

/// Cheap-clone handle held by the admission gate and the agent worker.
pub type SharedBillingService = Arc<dyn BillingService>;

/// Postgres-backed [`BillingService`]. Holds the [`SharedClock`] so the billing
/// period is deterministic under a `TestClock` (CLAUDE.md §11).
pub struct PgBillingService {
    pool: PgPool,
    clock: SharedClock,
}

impl PgBillingService {
    #[must_use]
    pub const fn new(pool: PgPool, clock: SharedClock) -> Self {
        Self { pool, clock }
    }
}

impl fmt::Debug for PgBillingService {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PgBillingService").finish_non_exhaustive()
    }
}

/// A point-in-time read of one org's billing: what it has spent this period and
/// its cap (`None` = unlimited).
#[derive(Debug, Clone, Copy)]
struct UsageSnapshot {
    used_micro_usd: i64,
    cap_micro_usd: Option<i64>,
}

impl UsageSnapshot {
    /// Turn a snapshot into a gate decision, recording the outcome on the
    /// current span.
    fn decide(self, org: OrgId) -> Result<(), BillingError> {
        let span = tracing::Span::current();
        span.record("patom.billing.used_micro", self.used_micro_usd);
        let Some(cap) = self.cap_micro_usd else {
            span.record("patom.billing.outcome", "unlimited");
            return Ok(());
        };
        // §6: a configured cap is positive (column CHECK) and usage never
        // negative (column CHECK); assert both so a corrupt read crashes here.
        assert!(cap > 0, "invariant: configured cap must be positive");
        assert!(self.used_micro_usd >= 0, "invariant: usage non-negative");
        span.record("patom.billing.cap_micro", cap);
        if self.used_micro_usd >= cap {
            span.record("patom.billing.outcome", "exceeded");
            return Err(BillingError::Exceeded {
                org,
                used_micro_usd: self.used_micro_usd,
                cap_micro_usd: cap,
            });
        }
        span.record("patom.billing.outcome", "ok");
        Ok(())
    }
}

/// Read the cap + current-period usage in one round-trip. An absent
/// `org_billing` row means unlimited (so usage is irrelevant); an absent
/// `org_billing_usage` row means zero spent.
async fn read_snapshot(
    tx: &mut Transaction<'_, Postgres>,
    org: OrgId,
    period: BillingPeriod,
) -> Result<UsageSnapshot, sqlx::Error> {
    let row: Option<(Option<i64>, Option<i64>)> = sqlx::query_as(GATE_SNAPSHOT)
        .bind(org)
        .bind(period.start_date())
        .fetch_optional(&mut **tx)
        .await?;
    let (cap, used) = row.map_or((None, None), |(c, u)| (c, u));
    Ok(UsageSnapshot {
        used_micro_usd: used.unwrap_or(0),
        cap_micro_usd: cap,
    })
}

/// Read one org's config (cap + warn threshold) plus the current period's usage
/// inside an open transaction, and assemble a [`BillingConfig`]. Shared by the
/// get (read-only) and set (read-after-write) paths so the assembly lives once.
async fn read_config(
    tx: &mut Transaction<'_, Postgres>,
    org: OrgId,
    period: BillingPeriod,
) -> Result<BillingConfig, sqlx::Error> {
    let (cap, bps, used, warned_at): (
        Option<i64>,
        Option<i32>,
        Option<i64>,
        Option<DateTime<Utc>>,
    ) = sqlx::query_as(READ_CONFIG)
        .bind(org)
        .bind(period.start_date())
        .fetch_one(&mut **tx)
        .await?;
    let used = used.unwrap_or(0);
    // §6: the column CHECK keeps stored usage non-negative; assert so a corrupt
    // read crashes here rather than underflowing `remaining`.
    assert!(used >= 0, "invariant: usage non-negative");
    // No config row → unlimited; surface the default threshold so the form has a
    // sensible starting value rather than a magic zero.
    let warn_threshold_bps = bps.map_or(DEFAULT_WARN_BPS, |b| {
        u16::try_from(b).unwrap_or(DEFAULT_WARN_BPS)
    });
    Ok(BillingConfig {
        cap_micro_usd: cap,
        warn_threshold_bps,
        used_micro_usd: used,
        warned_at,
        period_start: period.start_date(),
    })
}

/// Micro-USD at which the soft-warn fires: `cap * bps / 10000`. `i128`
/// intermediate so `cap * bps` cannot overflow before the divide.
fn warn_threshold_micros(cap: i64, bps: i32) -> i64 {
    assert!(cap > 0, "invariant: cap positive");
    assert!(bps > 0, "invariant: warn bps positive");
    let threshold = i128::from(cap) * i128::from(bps) / i128::from(WarnThresholdBps::FULL);
    // threshold <= cap (bps <= 10000), so it always fits i64.
    i64::try_from(threshold).expect("invariant: warn threshold <= cap fits i64")
}

impl PgBillingService {
    /// Shared gate body: read the snapshot inside `tx`, commit, decide. Both
    /// entry points differ only in how `tx` was opened (privileged vs RLS).
    #[tracing::instrument(
        skip_all,
        name = "billing.check",
        fields(
            patom.org.id = %org,
            patom.billing.outcome = tracing::field::Empty,
            patom.billing.used_micro = tracing::field::Empty,
            patom.billing.cap_micro = tracing::field::Empty,
        ),
    )]
    async fn gate(
        &self,
        org: OrgId,
        mut tx: Transaction<'_, Postgres>,
    ) -> Result<(), BillingError> {
        let period = BillingPeriod::current(&self.clock);
        let snapshot = read_snapshot(&mut tx, org, period).await?;
        tx.commit().await?;
        snapshot.decide(org)
    }
}

#[async_trait]
impl BillingService for PgBillingService {
    async fn check_or_fail(&self, org: OrgId) -> Result<(), BillingError> {
        let tx = begin_privileged(&self.pool).await?;
        self.gate(org, tx).await
    }

    async fn check_or_fail_for_user(
        &self,
        acting_user_id: UserId,
        org: OrgId,
    ) -> Result<(), BillingError> {
        let tx = begin_as_user(&self.pool, acting_user_id).await?;
        self.gate(org, tx).await
    }

    #[tracing::instrument(
        skip_all,
        name = "billing.settle",
        fields(
            patom.org.id = %org,
            patom.billing.cost_micro = cost.get(),
            patom.billing.used_micro = tracing::field::Empty,
            patom.billing.warned = tracing::field::Empty,
        ),
    )]
    async fn settle(&self, org: OrgId, cost: CostMicros) -> Result<(), BillingError> {
        let period = BillingPeriod::current(&self.clock);
        let now = self.clock.now_utc();
        let mut tx = begin_privileged(&self.pool).await?;
        // One round-trip: upsert the period total and read it back with the
        // org's cap + warn threshold (NULL when unconfigured / unlimited).
        let (used, cap, bps): (i64, Option<i64>, Option<i32>) = sqlx::query_as(SETTLE)
            .bind(org)
            .bind(period.start_date())
            .bind(cost.get())
            .bind(now)
            .fetch_one(&mut *tx)
            .await?;
        assert!(used >= 0, "invariant: period total never negative");
        let warned = fire_warn_once(&mut tx, org, period, used, cap, bps, now).await?;
        tx.commit().await?;
        let span = tracing::Span::current();
        span.record("patom.billing.used_micro", used);
        span.record("patom.billing.warned", warned);
        if warned {
            warn!(
                event = "billing.warn",
                patom.org.id = %org,
                patom.billing.used_micro = used,
                "org crossed its soft spend warn threshold this period"
            );
        }
        Ok(())
    }

    #[tracing::instrument(skip_all, name = "billing.get_config", fields(patom.org.id = %org))]
    async fn get_config(
        &self,
        acting_user_id: UserId,
        org: OrgId,
    ) -> Result<BillingConfig, BillingError> {
        let period = BillingPeriod::current(&self.clock);
        let mut tx = begin_as_user(&self.pool, acting_user_id).await?;
        let config = read_config(&mut tx, org, period).await?;
        tx.commit().await?;
        Ok(config)
    }

    #[tracing::instrument(skip_all, name = "billing.set_config", fields(patom.org.id = %org))]
    async fn set_config(
        &self,
        acting_user_id: UserId,
        org: OrgId,
        cap: Option<MonthlyCapMicros>,
        warn: WarnThresholdBps,
    ) -> Result<BillingConfig, BillingError> {
        let period = BillingPeriod::current(&self.clock);
        let now = self.clock.now_utc();
        let cap_micros = cap.map(MonthlyCapMicros::get);
        let bps = i32::from(warn.get());
        let mut tx = begin_as_user(&self.pool, acting_user_id).await?;
        sqlx::query(WRITE_CONFIG)
            .bind(org)
            .bind(cap_micros)
            .bind(bps)
            .bind(now)
            .execute(&mut *tx)
            .await?;
        // Read the fresh view back in the same transaction so the PUT returns it
        // without a second round-trip.
        let config = read_config(&mut tx, org, period).await?;
        tx.commit().await?;
        Ok(config)
    }
}

/// Fire the once-per-period warn if usage just crossed the threshold. Returns
/// `true` only for the settle that flipped `warned_at`. No-op when there's no
/// cap configured (unlimited orgs can't cross a threshold).
async fn fire_warn_once(
    tx: &mut Transaction<'_, Postgres>,
    org: OrgId,
    period: BillingPeriod,
    used: i64,
    cap: Option<i64>,
    bps: Option<i32>,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<bool, sqlx::Error> {
    let (Some(cap), Some(bps)) = (cap, bps) else {
        return Ok(false);
    };
    if used < warn_threshold_micros(cap, bps) {
        return Ok(false);
    }
    let result = sqlx::query(MARK_WARNED)
        .bind(org)
        .bind(period.start_date())
        .bind(now)
        .execute(&mut **tx)
        .await?;
    Ok(result.rows_affected() == 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn warn_threshold_is_cap_times_bps() {
        // 80% of $100 (100_000_000 micro) = $80 = 80_000_000 micro.
        assert_eq!(warn_threshold_micros(100_000_000, 8000), 80_000_000);
        // 100% threshold is the cap itself.
        assert_eq!(warn_threshold_micros(100_000_000, 10_000), 100_000_000);
        // 1% of an odd cap truncates toward zero.
        assert_eq!(warn_threshold_micros(999, 100), 9);
    }

    #[test]
    fn snapshot_unlimited_is_ok() {
        let org = OrgId::new();
        let snap = UsageSnapshot {
            used_micro_usd: 999_999,
            cap_micro_usd: None,
        };
        assert!(snap.decide(org).is_ok());
    }

    #[test]
    fn snapshot_under_cap_is_ok_and_at_cap_exceeds() {
        let org = OrgId::new();
        let under = UsageSnapshot {
            used_micro_usd: 99,
            cap_micro_usd: Some(100),
        };
        assert!(under.decide(org).is_ok());
        let at = UsageSnapshot {
            used_micro_usd: 100,
            cap_micro_usd: Some(100),
        };
        assert!(matches!(
            at.decide(org),
            Err(BillingError::Exceeded {
                used_micro_usd: 100,
                cap_micro_usd: 100,
                ..
            })
        ));
    }
}
