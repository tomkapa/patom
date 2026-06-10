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
use tracing::{info, warn};

use chrono::{DateTime, NaiveDate, Utc};

use crate::auth::{OrgId, UserId, begin_as_user, begin_privileged};
use crate::clock::SharedClock;
use crate::entitlements::{SharedEntitlements, UnlimitedEntitlements};
use crate::runtime::IdempotencyKey;

use super::error::BillingError;
use super::limits::DEFAULT_WARN_BPS;
use super::types::{
    BillingPeriod, CostMicros, CreditLedgerId, GrantAmount, LedgerDelta, LedgerKind, LedgerReason,
    MonthlyCapMicros, WarnThresholdBps,
};

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

/// Append one ledger entry, deduped by `idempotency_key`. `RETURNING id` yields
/// a row only when a *new* entry was written — `ON CONFLICT DO NOTHING` returns
/// nothing on replay, which is exactly the signal a grant uses to skip the
/// balance move. Usage debits pass a NULL key (never conflicts, always inserts).
const INSERT_LEDGER_ENTRY: &str = "
    INSERT INTO org_credit_ledger
        (id, org_id, delta_micro_usd, kind, reason, idempotency_key, actor, created_at)
    VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
    ON CONFLICT (idempotency_key) DO NOTHING
    RETURNING id";

/// Debit the materialized balance by a turn's cost and read the new totals back
/// for the `balance == granted − used` invariant (§6). Affects no row (returns
/// nothing) when the org has no `org_credits` row.
const SETTLE_DEBIT_CREDITS: &str = "
    UPDATE org_credits
       SET balance_micro_usd    = balance_micro_usd - $2,
           used_total_micro_usd = used_total_micro_usd + $2,
           updated_at           = $3
     WHERE org_id = $1
     RETURNING balance_micro_usd, granted_total_micro_usd, used_total_micro_usd";

/// Move the materialized balance by a grant: create the row on first credit or
/// add to the existing balance, and read the new totals back so the caller can
/// assert the `balance == granted − used` invariant (§6).
const GRANT_UPSERT_CREDITS: &str = "
    INSERT INTO org_credits
        (org_id, balance_micro_usd, granted_total_micro_usd, used_total_micro_usd, updated_at)
    VALUES ($1, $2, $2, 0, $3)
    ON CONFLICT (org_id) DO UPDATE
        SET balance_micro_usd       = org_credits.balance_micro_usd + EXCLUDED.balance_micro_usd,
            granted_total_micro_usd = org_credits.granted_total_micro_usd + EXCLUDED.granted_total_micro_usd,
            updated_at              = EXCLUDED.updated_at
    RETURNING balance_micro_usd, granted_total_micro_usd, used_total_micro_usd";

/// The org's materialized credit totals for the read API. Absent row → all zero.
const READ_CREDITS: &str = "
    SELECT balance_micro_usd, granted_total_micro_usd, used_total_micro_usd
    FROM org_credits WHERE org_id = $1";

/// The org's recent ledger entries, newest first, capped by `$2`
/// ([`super::limits::MAX_LEDGER_READ`]). Uses the `(org_id, created_at DESC)` index.
const READ_LEDGER: &str = "
    SELECT id, delta_micro_usd, kind, reason, created_at
    FROM org_credit_ledger WHERE org_id = $1
    ORDER BY created_at DESC LIMIT $2";

/// One credit ledger entry, for the read API.
#[derive(Debug, Clone)]
pub struct CreditLedgerEntry {
    pub id: CreditLedgerId,
    pub delta_micro_usd: i64,
    pub kind: LedgerKind,
    pub reason: LedgerReason,
    pub created_at: DateTime<Utc>,
}

/// One org's credit balance + recent ledger, for the read API. An org that has
/// never been granted credit reads as all-zero with an empty ledger.
#[derive(Debug, Clone)]
pub struct CreditSummary {
    pub balance_micro_usd: i64,
    pub granted_total_micro_usd: i64,
    pub used_total_micro_usd: i64,
    pub recent: Vec<CreditLedgerEntry>,
}

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

    /// Idempotently grant `amount` credit to `org` (privileged). Appends a
    /// `grant` ledger entry keyed by `key` and moves the materialized balance,
    /// both in one transaction. A repeat call with the same `key` is a no-op —
    /// the unique ledger insert gates the balance move — so a retried
    /// signup/promo grant never double-credits. `actor` is the user behind the
    /// grant, or `None` for a system grant.
    async fn grant_credit(
        &self,
        org: OrgId,
        amount: GrantAmount,
        reason: LedgerReason,
        key: &IdempotencyKey,
        actor: Option<UserId>,
    ) -> Result<(), BillingError>;

    /// Tenant-scoped read of the org's credit balance + recent ledger (#154).
    /// Opens `begin_as_user` so the read is RLS-filtered to the acting
    /// principal's org. `limit` caps the ledger slice
    /// ([`super::limits::MAX_LEDGER_READ`]). An org never granted credit reads
    /// as all-zero with an empty ledger.
    async fn read_credits(
        &self,
        acting_user_id: UserId,
        org: OrgId,
        limit: i64,
    ) -> Result<CreditSummary, BillingError>;

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

/// Postgres-backed [`BillingService`].
///
/// Holds the [`SharedClock`] so the billing period is deterministic under a
/// `TestClock` (CLAUDE.md §11), and the [`SharedEntitlements`] policy so the
/// gate knows whether the free-credit gate is active for an org (#154).
pub struct PgBillingService {
    pool: PgPool,
    clock: SharedClock,
    entitlements: SharedEntitlements,
}

impl PgBillingService {
    /// Build a service whose credit gate is **inactive** (the OSS / self-host
    /// default). The cap gate still applies; the credit balance is ignored.
    /// Production wires the real policy via [`Self::with_entitlements`].
    #[must_use]
    pub fn new(pool: PgPool, clock: SharedClock) -> Self {
        Self::with_entitlements(pool, clock, Arc::new(UnlimitedEntitlements))
    }

    /// Build a service with an explicit entitlement policy — the cloud path,
    /// where `entitlements.credit_gate_active` decides whether a zero balance
    /// blocks a turn.
    #[must_use]
    pub fn with_entitlements(
        pool: PgPool,
        clock: SharedClock,
        entitlements: SharedEntitlements,
    ) -> Self {
        Self {
            pool,
            clock,
            entitlements,
        }
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

/// The org's materialized credit balance for the gate. An absent `org_credits`
/// row reads as zero (see [`read_credit_balance`]).
const CREDIT_BALANCE: &str = "SELECT balance_micro_usd FROM org_credits WHERE org_id = $1";

/// Read the org's credit balance in micro-USD inside the open transaction. An
/// absent `org_credits` row reads as **zero**, not unlimited: a credit-gated org
/// with no grant (pre-grant or post-promo) is out of credit. Under `begin_as_user`
/// the read is RLS-filtered, so a cross-tenant org argument also reads as zero.
async fn read_credit_balance(
    tx: &mut Transaction<'_, Postgres>,
    org: OrgId,
) -> Result<i64, sqlx::Error> {
    let row: Option<(i64,)> = sqlx::query_as(CREDIT_BALANCE)
        .bind(org)
        .fetch_optional(&mut **tx)
        .await?;
    Ok(row.map_or(0, |(b,)| b))
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
            patom.credit.balance = tracing::field::Empty,
            patom.credit.outcome = tracing::field::Empty,
        ),
    )]
    async fn gate(
        &self,
        org: OrgId,
        mut tx: Transaction<'_, Postgres>,
    ) -> Result<(), BillingError> {
        let period = BillingPeriod::current(&self.clock);
        let snapshot = read_snapshot(&mut tx, org, period).await?;
        // Credit gate (#154): read + enforce only when policy says it is active
        // for this org. Under the OSS default it never is, so this is skipped
        // and the credit balance is never even read.
        let credit_balance = if self.entitlements.credit_gate_active(org) {
            Some(read_credit_balance(&mut tx, org).await?)
        } else {
            None
        };
        tx.commit().await?;
        // Cap first (→ 429 Exceeded), then credit exhaustion (→ 402 OutOfCredit):
        // two distinct failure modes, reported separately.
        snapshot.decide(org)?;
        if let Some(balance) = credit_balance {
            let span = tracing::Span::current();
            span.record("patom.credit.balance", balance);
            if balance <= 0 {
                span.record("patom.credit.outcome", "out_of_credit");
                // Exhaustion counter (#154): one event per blocked turn. The
                // `patom.credit.outcome` span field carries the same signal for
                // trace-side aggregation; no PII.
                info!(
                    event = "credit.exhausted",
                    patom.org.id = %org,
                    patom.credit.balance = balance,
                );
                return Err(BillingError::OutOfCredit {
                    org,
                    balance_micro_usd: balance,
                });
            }
            span.record("patom.credit.outcome", "ok");
        }
        Ok(())
    }

    /// Debit this turn's `cost` from the org's credit balance inside the open
    /// settle transaction, and append the matching `usage` ledger entry — but
    /// only when the credit gate is active for the org and the turn cost a
    /// nonzero amount. A missing `org_credits` row (a pre-grant / OSS org) is a
    /// no-op. Usage debits carry a NULL idempotency key (not deduped); the gate
    /// already bounds the single-turn overrun, so a post-paid dip is acceptable.
    async fn debit_credits(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        org: OrgId,
        cost: CostMicros,
        now: DateTime<Utc>,
    ) -> Result<(), BillingError> {
        if !self.entitlements.credit_gate_active(org) || cost.get() == 0 {
            return Ok(());
        }
        let debited: Option<(i64, i64, i64)> = sqlx::query_as(SETTLE_DEBIT_CREDITS)
            .bind(org)
            .bind(cost.get())
            .bind(now)
            .fetch_optional(&mut **tx)
            .await?;
        let Some((balance, granted, used)) = debited else {
            return Ok(());
        };
        sqlx::query(INSERT_LEDGER_ENTRY)
            .bind(CreditLedgerId::new())
            .bind(org)
            .bind(LedgerDelta::from(cost).get())
            .bind(LedgerKind::Debit.as_str())
            .bind(LedgerReason::Usage.as_str())
            .bind(None::<&str>)
            .bind(None::<UserId>)
            .bind(now)
            .execute(&mut **tx)
            .await?;
        // §6: the column CHECKs keep the totals non-negative; assert the balance
        // identity so a corrupt materialization crashes here.
        assert!(granted >= 0, "invariant: granted_total non-negative");
        assert!(used >= 0, "invariant: used_total non-negative");
        assert_eq!(
            balance,
            granted - used,
            "invariant: balance == granted - used"
        );
        // Balance gauge (#154): the post-debit balance is the latest value for a
        // per-org "credit remaining" gauge; the debit amount is the spend rate.
        info!(
            event = "credit.debit",
            patom.org.id = %org,
            patom.credit.debit_micro = cost.get(),
            patom.credit.balance = balance,
        );
        Ok(())
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
        // Credit debit (#154) in the *same* tx, so the cap usage and the credit
        // balance move atomically. Inert under the OSS default (gate inactive).
        self.debit_credits(&mut tx, org, cost, now).await?;
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

    #[tracing::instrument(
        skip_all,
        name = "billing.grant",
        fields(
            patom.org.id = %org,
            patom.billing.grant_micros = amount.get(),
            patom.billing.reason = reason.as_str(),
            patom.billing.granted = tracing::field::Empty,
        ),
    )]
    async fn grant_credit(
        &self,
        org: OrgId,
        amount: GrantAmount,
        reason: LedgerReason,
        key: &IdempotencyKey,
        actor: Option<UserId>,
    ) -> Result<(), BillingError> {
        let now = self.clock.now_utc();
        let delta = LedgerDelta::from(amount).get();
        // §6: a grant is always a positive movement (GrantAmount is `> 0`).
        assert!(delta > 0, "invariant: a grant is a positive delta");
        let mut tx = begin_privileged(&self.pool).await?;
        // Insert-gated idempotency: the unique `idempotency_key` makes the
        // INSERT the dedup point. Only when a *new* row is written do we move
        // the balance, so a replayed grant (same key) is a true no-op.
        let inserted: Option<(CreditLedgerId,)> = sqlx::query_as(INSERT_LEDGER_ENTRY)
            .bind(CreditLedgerId::new())
            .bind(org)
            .bind(delta)
            .bind(LedgerKind::Grant.as_str())
            .bind(reason.as_str())
            .bind(key.as_str())
            .bind(actor)
            .bind(now)
            .fetch_optional(&mut *tx)
            .await?;
        let granted = if inserted.is_some() {
            let (balance, granted_total, used_total): (i64, i64, i64) =
                sqlx::query_as(GRANT_UPSERT_CREDITS)
                    .bind(org)
                    .bind(amount.get())
                    .bind(now)
                    .fetch_one(&mut *tx)
                    .await?;
            // §6: the column CHECKs keep the totals non-negative; assert the
            // balance identity so a corrupt materialization crashes here.
            assert!(granted_total >= 0, "invariant: granted_total non-negative");
            assert!(used_total >= 0, "invariant: used_total non-negative");
            assert_eq!(
                balance,
                granted_total - used_total,
                "invariant: balance == granted - used"
            );
            true
        } else {
            false
        };
        tx.commit().await?;
        tracing::Span::current().record("patom.billing.granted", granted);
        // Grant counter by reason (#154): one event per grant, dimensioned by
        // `reason` so promo/referral/signup volume is separable. `applied=false`
        // means the idempotency key already existed (a replay). No PII.
        info!(
            event = "credit.grant",
            patom.org.id = %org,
            patom.credit.reason = reason.as_str(),
            patom.credit.amount_micro = amount.get(),
            patom.credit.applied = granted,
        );
        Ok(())
    }

    #[tracing::instrument(skip_all, name = "billing.read_credits", fields(patom.org.id = %org))]
    async fn read_credits(
        &self,
        acting_user_id: UserId,
        org: OrgId,
        limit: i64,
    ) -> Result<CreditSummary, BillingError> {
        assert!(limit > 0, "invariant: ledger read limit is positive");
        let mut tx = begin_as_user(&self.pool, acting_user_id).await?;
        let totals: Option<(i64, i64, i64)> = sqlx::query_as(READ_CREDITS)
            .bind(org)
            .fetch_optional(&mut *tx)
            .await?;
        let rows: Vec<(CreditLedgerId, i64, LedgerKind, LedgerReason, DateTime<Utc>)> =
            sqlx::query_as(READ_LEDGER)
                .bind(org)
                .bind(limit)
                .fetch_all(&mut *tx)
                .await?;
        tx.commit().await?;
        let (balance, granted, used) = totals.unwrap_or((0, 0, 0));
        // §6: an absent row reads as zero; a present one obeys the column CHECKs.
        assert!(granted >= 0, "invariant: granted_total non-negative");
        assert!(used >= 0, "invariant: used_total non-negative");
        let recent = rows
            .into_iter()
            .map(
                |(id, delta_micro_usd, kind, reason, created_at)| CreditLedgerEntry {
                    id,
                    delta_micro_usd,
                    kind,
                    reason,
                    created_at,
                },
            )
            .collect();
        Ok(CreditSummary {
            balance_micro_usd: balance,
            granted_total_micro_usd: granted,
            used_total_micro_usd: used,
            recent,
        })
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
