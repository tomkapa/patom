//! Trait-contract tests for [`BillingService`] / [`PgBillingService`].
//!
//! Covers the post-paid budget semantics: atomic settle under concurrency, the
//! gate blocking at the cap, unlimited orgs, monthly-period rollover (via
//! `TestClock`), the once-per-period soft warn, and RLS isolation of the
//! tenant-scoped gate.

#![allow(clippy::expect_used)]

use std::sync::Arc;
use std::time::Duration;

use patom::auth::OrgId;
use patom::billing::{BillingError, BillingService, CostMicros, PgBillingService};
use patom::clock::{SharedClock, SystemClock, TestClock};
use sqlx::PgPool;

mod common;
use common::pg::{seed_tenant, set_billing};

fn cost(micros: i64) -> CostMicros {
    CostMicros::try_from(micros).expect("non-negative cost")
}

/// Current-period total + warned_at for assertions, read RLS-bypassing.
async fn read_usage(
    pool: &PgPool,
    org: OrgId,
) -> Option<(i64, Option<chrono::DateTime<chrono::Utc>>)> {
    sqlx::query_as(
        "SELECT used_micro_usd, warned_at FROM org_billing_usage
         WHERE org_id = $1 ORDER BY period_start DESC LIMIT 1",
    )
    .bind(org)
    .fetch_optional(pool)
    .await
    .expect("read usage")
}

#[sqlx::test]
async fn settle_accumulates_atomically_under_concurrency(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let service = Arc::new(PgBillingService::new(pool.clone(), SystemClock::shared()));

    // 20 concurrent settles of 1000 micro-USD each must total exactly 20_000 —
    // the ON CONFLICT increment cannot lose an update.
    let mut handles = Vec::new();
    for _ in 0..20 {
        let svc = service.clone();
        let org = seed.org_id;
        handles.push(tokio::spawn(async move {
            svc.settle(org, cost(1000)).await.expect("settle");
        }));
    }
    for h in handles {
        h.await.expect("join");
    }

    let (used, _) = read_usage(&pool, seed.org_id).await.expect("usage row");
    assert_eq!(used, 20_000);
}

#[sqlx::test]
async fn gate_blocks_at_cap_and_passes_below(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let service = PgBillingService::new(pool.clone(), SystemClock::shared());
    set_billing(&pool, seed.org_id, Some(5_000), 8000).await;

    service
        .settle(seed.org_id, cost(3_000))
        .await
        .expect("settle 1");
    service
        .check_or_fail(seed.org_id)
        .await
        .expect("under cap passes");

    service
        .settle(seed.org_id, cost(2_500))
        .await
        .expect("settle 2");
    let err = service
        .check_or_fail(seed.org_id)
        .await
        .expect_err("over cap blocks");
    assert!(matches!(
        err,
        BillingError::Exceeded {
            used_micro_usd: 5_500,
            cap_micro_usd: 5_000,
            ..
        }
    ));
}

#[sqlx::test]
async fn gate_unlimited_without_configured_cap(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let service = PgBillingService::new(pool.clone(), SystemClock::shared());
    // No org_billing row at all → unlimited.
    service
        .settle(seed.org_id, cost(1_000_000_000))
        .await
        .expect("settle");
    service
        .check_or_fail(seed.org_id)
        .await
        .expect("unlimited org never blocks");
}

#[sqlx::test]
// from_hours/from_days (the lint's suggestion) are unstable on this toolchain;
// from_mins is the largest stable unit available.
#[allow(clippy::duration_suboptimal_units)]
async fn period_rollover_starts_a_fresh_counter(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let test_clock = Arc::new(TestClock::new());
    let clock: SharedClock = test_clock.clone();
    let service = PgBillingService::new(pool.clone(), clock);
    set_billing(&pool, seed.org_id, Some(3_000), 8000).await;

    // Month A: spend over the cap → blocked.
    service
        .settle(seed.org_id, cost(4_000))
        .await
        .expect("settle A");
    assert!(service.check_or_fail(seed.org_id).await.is_err());

    // Cross into a new month — ~40 days guarantees a boundary on any start day.
    test_clock.advance(Duration::from_mins(57_600)); // 40 days

    // Month B: the new (org, period) row starts at zero → passes.
    service
        .check_or_fail(seed.org_id)
        .await
        .expect("new period resets usage");

    let (rows,): (i64,) =
        sqlx::query_as("SELECT count(*) FROM org_billing_usage WHERE org_id = $1")
            .bind(seed.org_id)
            .fetch_one(&pool)
            .await
            .expect("count rows");
    assert_eq!(rows, 1, "only month A has a usage row until B settles");
}

#[sqlx::test]
async fn warn_fires_once_per_period(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let test_clock = Arc::new(TestClock::new());
    let clock: SharedClock = test_clock.clone();
    let service = PgBillingService::new(pool.clone(), clock);
    // cap $0.10 (100_000 micro), warn at 80% → threshold 80_000.
    set_billing(&pool, seed.org_id, Some(100_000), 8000).await;

    service
        .settle(seed.org_id, cost(50_000))
        .await
        .expect("settle 1");
    let (_, warned) = read_usage(&pool, seed.org_id).await.expect("row");
    assert!(warned.is_none(), "below threshold: no warn yet");

    test_clock.advance(Duration::from_secs(10));
    service
        .settle(seed.org_id, cost(40_000))
        .await
        .expect("settle 2");
    let (used, warned_first) = read_usage(&pool, seed.org_id).await.expect("row");
    assert_eq!(used, 90_000);
    let warned_first = warned_first.expect("crossed threshold: warned_at set");

    // A later settle in the same period must NOT move warned_at (fires once).
    test_clock.advance(Duration::from_secs(10));
    service
        .settle(seed.org_id, cost(5_000))
        .await
        .expect("settle 3");
    let (_, warned_again) = read_usage(&pool, seed.org_id).await.expect("row");
    assert_eq!(
        warned_again.expect("still warned"),
        warned_first,
        "warn timestamp is stable once set"
    );
}

#[sqlx::test]
async fn tenant_gate_is_rls_isolated(pool: PgPool) {
    // Two independent tenants. A is over its cap; B is a stranger to A.
    let a = seed_tenant(&pool).await;
    let b = seed_tenant(&pool).await;
    let service = PgBillingService::new(pool.clone(), SystemClock::shared());
    set_billing(&pool, a.org_id, Some(1_000), 8000).await;
    service
        .settle(a.org_id, cost(2_000))
        .await
        .expect("settle A over cap");

    // A's own member sees the cap and is blocked.
    let err = service
        .check_or_fail_for_user(a.user_id, a.org_id)
        .await
        .expect_err("A's user is blocked");
    assert!(matches!(err, BillingError::Exceeded { .. }));

    // B's user cannot read A's budget row (RLS) → reads as unlimited → passes.
    service
        .check_or_fail_for_user(b.user_id, a.org_id)
        .await
        .expect("cross-tenant read sees no cap");

    // The privileged worker gate bypasses RLS and still blocks A.
    assert!(service.check_or_fail(a.org_id).await.is_err());
}
