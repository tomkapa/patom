//! Settle debits credits (#154 S7): when the credit gate is active, the
//! post-paid `settle` also moves the materialized balance and appends a `usage`
//! ledger entry in the same transaction; under the OSS default it leaves credits
//! untouched.

#![allow(clippy::expect_used)]

use std::sync::Arc;

use patom::auth::OrgId;
use patom::billing::{BillingService, CostMicros, GrantAmount, LedgerReason, PgBillingService};
use patom::clock::SystemClock;
use patom::entitlements::{AgentLimit, Entitlements, Feature, SharedEntitlements};
use patom::runtime::IdempotencyKey;
use sqlx::PgPool;

mod common;
use common::pg::seed_tenant;

/// Credit gate ON (cloud shape), no auto signup grant.
#[derive(Debug)]
struct ActiveCreditPolicy;

impl Entitlements for ActiveCreditPolicy {
    fn agent_limit(&self, _org: OrgId) -> AgentLimit {
        AgentLimit::Unlimited
    }
    fn allows(&self, _org: OrgId, _feature: Feature) -> bool {
        true
    }
    fn credit_gate_active(&self, _org: OrgId) -> bool {
        true
    }
    fn signup_grant(&self, _org: OrgId) -> Option<GrantAmount> {
        None
    }
}

fn active_service(pool: &PgPool) -> PgBillingService {
    let policy: SharedEntitlements = Arc::new(ActiveCreditPolicy);
    PgBillingService::with_entitlements(pool.clone(), SystemClock::shared(), policy)
}

fn cost(micros: i64) -> CostMicros {
    CostMicros::try_from(micros).expect("non-negative cost")
}

async fn grant(service: &PgBillingService, org: OrgId, micros: i64) {
    service
        .grant_credit(
            org,
            GrantAmount::try_from(micros).expect("positive"),
            LedgerReason::Manual,
            &IdempotencyKey::try_from(format!("seed:{}", org.as_uuid())).expect("key"),
            None,
        )
        .await
        .expect("grant");
}

async fn credits(pool: &PgPool, org: OrgId) -> Option<(i64, i64, i64)> {
    sqlx::query_as(
        "SELECT balance_micro_usd, granted_total_micro_usd, used_total_micro_usd \
         FROM org_credits WHERE org_id = $1",
    )
    .bind(org)
    .fetch_optional(pool)
    .await
    .expect("read credits")
}

async fn usage_entries(pool: &PgPool, org: OrgId) -> Vec<i64> {
    sqlx::query_scalar(
        "SELECT delta_micro_usd FROM org_credit_ledger \
         WHERE org_id = $1 AND kind = 'debit' AND reason = 'usage' ORDER BY created_at",
    )
    .bind(org)
    .fetch_all(pool)
    .await
    .expect("read usage entries")
}

#[sqlx::test]
async fn settle_debits_balance_and_appends_usage_entry(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let service = active_service(&pool);
    grant(&service, seed.org_id, 1_000_000).await;

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
    grant(&service, seed.org_id, 100_000).await;

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
    grant(&service, seed.org_id, 1_000_000).await;

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
    grant(&service, seed.org_id, 1_000_000).await;

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
