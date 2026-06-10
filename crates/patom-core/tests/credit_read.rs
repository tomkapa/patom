//! Tenant-scoped credit read API (#154 S9): `BillingService::read_credits`
//! returns the materialized balance plus a capped, newest-first ledger slice,
//! RLS-filtered to the acting principal's org.

#![allow(clippy::expect_used)]

use std::sync::Arc;
use std::time::Duration;

use patom::auth::OrgId;
use patom::billing::{BillingService, GrantAmount, LedgerReason, PgBillingService};
use patom::clock::{SharedClock, TestClock};
use patom::runtime::IdempotencyKey;
use sqlx::PgPool;

mod common;
use common::pg::seed_tenant;

async fn grant(service: &PgBillingService, org: OrgId, micros: i64, key: &str) {
    service
        .grant_credit(
            org,
            GrantAmount::try_from(micros).expect("positive"),
            LedgerReason::Manual,
            &IdempotencyKey::try_from(key.to_owned()).expect("key"),
            None,
        )
        .await
        .expect("grant");
}

#[sqlx::test]
async fn read_credits_returns_balance_and_newest_first_ledger(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let test_clock = Arc::new(TestClock::new());
    let clock: SharedClock = test_clock.clone();
    let service = PgBillingService::new(pool.clone(), clock);

    grant(&service, seed.org_id, 1_000_000, "g1").await;
    test_clock.advance(Duration::from_secs(1));
    grant(&service, seed.org_id, 500_000, "g2").await;
    test_clock.advance(Duration::from_secs(1));
    grant(&service, seed.org_id, 250_000, "g3").await;

    let summary = service
        .read_credits(seed.user_id, seed.org_id, 50)
        .await
        .expect("read");
    assert_eq!(summary.balance_micro_usd, 1_750_000);
    assert_eq!(summary.granted_total_micro_usd, 1_750_000);
    assert_eq!(summary.used_total_micro_usd, 0);
    // Newest first.
    let deltas: Vec<i64> = summary.recent.iter().map(|e| e.delta_micro_usd).collect();
    assert_eq!(deltas, vec![250_000, 500_000, 1_000_000]);
}

#[sqlx::test]
async fn read_credits_caps_the_ledger_slice(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let test_clock = Arc::new(TestClock::new());
    let clock: SharedClock = test_clock.clone();
    let service = PgBillingService::new(pool.clone(), clock);

    for i in 0..5 {
        grant(&service, seed.org_id, 1_000, &format!("g{i}")).await;
        test_clock.advance(Duration::from_secs(1));
    }
    // limit = 2 → only the two newest entries.
    let summary = service
        .read_credits(seed.user_id, seed.org_id, 2)
        .await
        .expect("read");
    assert_eq!(summary.recent.len(), 2);
    assert_eq!(
        summary.balance_micro_usd, 5_000,
        "balance still totals all grants"
    );
}

#[sqlx::test]
async fn read_credits_is_all_zero_without_grants(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let service = PgBillingService::new(pool.clone(), patom::clock::SystemClock::shared());
    let summary = service
        .read_credits(seed.user_id, seed.org_id, 50)
        .await
        .expect("read");
    assert_eq!(summary.balance_micro_usd, 0);
    assert_eq!(summary.granted_total_micro_usd, 0);
    assert!(summary.recent.is_empty());
}

#[sqlx::test]
async fn read_credits_is_rls_isolated(pool: PgPool) {
    let a = seed_tenant(&pool).await;
    let b = seed_tenant(&pool).await;
    let service = PgBillingService::new(pool.clone(), patom::clock::SystemClock::shared());
    grant(&service, a.org_id, 1_000_000, "a-grant").await;

    // B's member reading A's org sees nothing (RLS filters by membership).
    let summary = service
        .read_credits(b.user_id, a.org_id, 50)
        .await
        .expect("read");
    assert_eq!(summary.balance_micro_usd, 0);
    assert!(summary.recent.is_empty());
}
