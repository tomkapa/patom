//! Settle debits credits (#154 S7): when the credit gate is active, the
//! post-paid `settle` also moves the materialized balance and appends a `usage`
//! ledger entry in the same transaction; under the OSS default it leaves credits
//! untouched.

#![allow(clippy::expect_used)]

use patom::billing::{BillingService, CostMicros, PgBillingService};
use patom::clock::SystemClock;
use sqlx::PgPool;

mod common;
use common::billing::{active_service, cost, grant, read_org_credits as credits, usage_entries};
use common::pg::seed_tenant;

#[sqlx::test]
async fn settle_debits_balance_and_appends_usage_entry(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let service = active_service(&pool);
    grant(&service, seed.org_id, 1_000_000, "seed").await;

    service
        .settle(seed.org_id, cost(300_000))
        .await
        .expect("settle");

    let (balance, granted, used) = credits(&pool, seed.org_id).await.expect("credits row");
    assert_eq!(balance, 700_000);
    assert_eq!(granted, 1_000_000);
    assert_eq!(used, 300_000);
    assert_eq!(balance, granted - used);
    assert_eq!(usage_entries(&pool, seed.org_id).await, vec![-300_000]);
}

#[sqlx::test]
async fn settle_may_dip_balance_negative(pool: PgPool) {
    // Post-paid: the final turn can overshoot a near-zero balance. Acceptable —
    // the gate blocks the *next* turn.
    let seed = seed_tenant(&pool).await;
    let service = active_service(&pool);
    grant(&service, seed.org_id, 100_000, "seed").await;

    service
        .settle(seed.org_id, cost(150_000))
        .await
        .expect("settle");

    let (balance, _granted, used) = credits(&pool, seed.org_id).await.expect("credits row");
    assert_eq!(balance, -50_000);
    assert_eq!(used, 150_000);
}

#[sqlx::test]
async fn inactive_gate_settle_leaves_credits_untouched(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    // Default service = UnlimitedEntitlements → credit gate inactive.
    let service = PgBillingService::new(pool.clone(), SystemClock::shared());
    grant(&service, seed.org_id, 1_000_000, "seed").await;

    service
        .settle(seed.org_id, cost(300_000))
        .await
        .expect("settle");

    let (balance, _granted, used) = credits(&pool, seed.org_id).await.expect("credits row");
    assert_eq!(balance, 1_000_000, "inactive gate never debits");
    assert_eq!(used, 0);
    assert!(usage_entries(&pool, seed.org_id).await.is_empty());
}

#[sqlx::test]
async fn zero_cost_settle_appends_no_usage_entry(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let service = active_service(&pool);
    grant(&service, seed.org_id, 1_000_000, "seed").await;

    service
        .settle(seed.org_id, CostMicros::ZERO)
        .await
        .expect("settle");

    let (balance, _granted, used) = credits(&pool, seed.org_id).await.expect("credits row");
    assert_eq!(balance, 1_000_000);
    assert_eq!(used, 0);
    assert!(usage_entries(&pool, seed.org_id).await.is_empty());
}

#[sqlx::test]
async fn settle_without_credits_row_is_a_noop(pool: PgPool) {
    // Active gate but no grant yet → no org_credits row. Settle must not crash
    // or create a row; the cap-usage path still runs.
    let seed = seed_tenant(&pool).await;
    let service = active_service(&pool);

    service
        .settle(seed.org_id, cost(300_000))
        .await
        .expect("settle");

    assert!(credits(&pool, seed.org_id).await.is_none());
    assert!(usage_entries(&pool, seed.org_id).await.is_empty());
}
