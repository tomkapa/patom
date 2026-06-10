//! Trait-contract tests for [`BillingService::grant_credit`] (#154 S3): the
//! append + materialize primitive and its insert-gated idempotency.

#![allow(clippy::expect_used)]

use std::sync::Arc;

use patom::auth::OrgId;
use patom::billing::{BillingService, LedgerReason, PgBillingService};
use patom::clock::SystemClock;
use sqlx::PgPool;

mod common;
use common::billing::{grant_amount as grant, idem_key as key, read_org_credits as read_credits};
use common::pg::seed_tenant;

async fn ledger_count(pool: &PgPool, org: OrgId) -> i64 {
    let (n,): (i64,) = sqlx::query_as("SELECT count(*) FROM org_credit_ledger WHERE org_id = $1")
        .bind(org)
        .fetch_one(pool)
        .await
        .expect("count ledger");
    n
}

#[sqlx::test]
async fn grant_credits_balance_and_appends_ledger(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let service = PgBillingService::new(pool.clone(), SystemClock::shared());

    service
        .grant_credit(
            seed.org_id,
            grant(2_000_000),
            LedgerReason::SignupBonus,
            &key(&format!("signup:{}", seed.org_id.as_uuid())),
            None,
        )
        .await
        .expect("grant");

    let (balance, granted, used) = read_credits(&pool, seed.org_id).await.expect("credits row");
    assert_eq!(balance, 2_000_000);
    assert_eq!(granted, 2_000_000);
    assert_eq!(used, 0);
    assert_eq!(ledger_count(&pool, seed.org_id).await, 1);

    // The ledger row carries the signed delta, kind, and reason.
    let (delta, kind, reason): (i64, String, String) = sqlx::query_as(
        "SELECT delta_micro_usd, kind, reason FROM org_credit_ledger WHERE org_id = $1",
    )
    .bind(seed.org_id)
    .fetch_one(&pool)
    .await
    .expect("read ledger");
    assert_eq!(delta, 2_000_000);
    assert_eq!(kind, "grant");
    assert_eq!(reason, "signup_bonus");
}

#[sqlx::test]
async fn grant_is_idempotent_under_same_key(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let service = PgBillingService::new(pool.clone(), SystemClock::shared());
    let k = key(&format!("signup:{}", seed.org_id.as_uuid()));

    // Three grants with the SAME key must credit exactly once.
    for _ in 0..3 {
        service
            .grant_credit(
                seed.org_id,
                grant(2_000_000),
                LedgerReason::SignupBonus,
                &k,
                None,
            )
            .await
            .expect("grant");
    }

    let (balance, granted, used) = read_credits(&pool, seed.org_id).await.expect("credits row");
    assert_eq!(balance, 2_000_000, "balance counted once");
    assert_eq!(granted, 2_000_000);
    assert_eq!(used, 0);
    assert_eq!(
        ledger_count(&pool, seed.org_id).await,
        1,
        "only one ledger entry for the repeated key"
    );
}

#[sqlx::test]
async fn grants_with_distinct_keys_accumulate(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let service = PgBillingService::new(pool.clone(), SystemClock::shared());

    service
        .grant_credit(
            seed.org_id,
            grant(2_000_000),
            LedgerReason::SignupBonus,
            &key("signup:x"),
            None,
        )
        .await
        .expect("grant 1");
    service
        .grant_credit(
            seed.org_id,
            grant(500_000),
            LedgerReason::Promo,
            &key("promo:welcome"),
            None,
        )
        .await
        .expect("grant 2");

    let (balance, granted, used) = read_credits(&pool, seed.org_id).await.expect("credits row");
    assert_eq!(balance, 2_500_000);
    assert_eq!(granted, 2_500_000);
    assert_eq!(used, 0);
    assert_eq!(ledger_count(&pool, seed.org_id).await, 2);
}

#[sqlx::test]
async fn concurrent_distinct_grants_are_atomic(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let service = Arc::new(PgBillingService::new(pool.clone(), SystemClock::shared()));

    // 20 concurrent grants of 1000 each, each with its own key, must total
    // exactly 20_000 — the ON CONFLICT balance increment cannot lose an update.
    let mut handles = Vec::new();
    for i in 0..20 {
        let svc = service.clone();
        let org = seed.org_id;
        handles.push(tokio::spawn(async move {
            svc.grant_credit(
                org,
                grant(1_000),
                LedgerReason::Manual,
                &key(&format!("grant:{i}")),
                None,
            )
            .await
            .expect("grant");
        }));
    }
    for h in handles {
        h.await.expect("join");
    }

    let (balance, granted, used) = read_credits(&pool, seed.org_id).await.expect("credits row");
    assert_eq!(balance, 20_000);
    assert_eq!(granted, 20_000);
    assert_eq!(used, 0);
    assert_eq!(ledger_count(&pool, seed.org_id).await, 20);
}
