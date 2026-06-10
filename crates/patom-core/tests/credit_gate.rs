//! The free-credit gate (#154 S6): `BillingService::check_or_fail[_for_user]`
//! blocks a turn with `OutOfCredit` when the credit gate is active for an org
//! and its balance is at or below zero, passes when it has credit, and is inert
//! (ignores the balance) when policy says the gate is off — plus the HTTP 402
//! mapping.

#![allow(clippy::expect_used)]

use std::sync::Arc;

use axum::response::IntoResponse;
use patom::auth::{OrgId, begin_privileged};
use patom::billing::{BillingError, BillingService, GrantAmount, LedgerReason, PgBillingService};
use patom::clock::SystemClock;
use patom::entitlements::{AgentLimit, Entitlements, Feature, SharedEntitlements};
use patom::http::HttpError;
use patom::runtime::IdempotencyKey;
use sqlx::PgPool;

mod common;
use common::pg::seed_tenant;

/// A policy with the credit gate **on** (the cloud shape) but no signup grant —
/// so a fresh org starts at zero and is blocked until it is granted credit.
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

async fn grant(service: &PgBillingService, org: OrgId, micros: i64, key: &str) {
    service
        .grant_credit(
            org,
            GrantAmount::try_from(micros).expect("positive"),
            LedgerReason::Manual,
            &IdempotencyKey::try_from(key.to_owned()).expect("valid key"),
            None,
        )
        .await
        .expect("grant");
}

#[sqlx::test]
async fn active_gate_blocks_when_no_credit_row(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let service = active_service(&pool);
    // No org_credits row → balance reads as zero → blocked.
    let err = service
        .check_or_fail(seed.org_id)
        .await
        .expect_err("zero balance blocks");
    assert!(matches!(
        err,
        BillingError::OutOfCredit {
            balance_micro_usd: 0,
            ..
        }
    ));
}

#[sqlx::test]
async fn active_gate_passes_with_positive_balance(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let service = active_service(&pool);
    grant(&service, seed.org_id, 1_000_000, "seed").await;
    service
        .check_or_fail(seed.org_id)
        .await
        .expect("positive balance passes");
}

#[sqlx::test]
async fn active_gate_blocks_negative_balance(pool: PgPool) {
    // Post-paid settle can dip the final turn slightly negative; the next gate
    // must then block.
    let seed = seed_tenant(&pool).await;
    let mut tx = begin_privileged(&pool).await.expect("tx");
    sqlx::query(
        "INSERT INTO org_credits
             (org_id, balance_micro_usd, granted_total_micro_usd, used_total_micro_usd, updated_at)
         VALUES ($1, -250, 1000, 1250, now())",
    )
    .bind(seed.org_id)
    .execute(&mut *tx)
    .await
    .expect("seed negative balance");
    tx.commit().await.expect("commit");

    let service = active_service(&pool);
    let err = service
        .check_or_fail(seed.org_id)
        .await
        .expect_err("negative balance blocks");
    assert!(matches!(
        err,
        BillingError::OutOfCredit {
            balance_micro_usd: -250,
            ..
        }
    ));
}

#[sqlx::test]
async fn inactive_gate_ignores_zero_balance(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    // Default service = UnlimitedEntitlements → credit gate inactive.
    let service = PgBillingService::new(pool.clone(), SystemClock::shared());
    // No credits row, zero balance — but the gate never reads it.
    service
        .check_or_fail(seed.org_id)
        .await
        .expect("inactive gate ignores credit balance");
}

#[sqlx::test]
async fn active_gate_rls_path_blocks_then_passes(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let service = active_service(&pool);

    // Admission gate (RLS, acting user) blocks at zero...
    let err = service
        .check_or_fail_for_user(seed.user_id, seed.org_id)
        .await
        .expect_err("RLS gate blocks at zero");
    assert!(matches!(err, BillingError::OutOfCredit { .. }));

    // ...and passes once the org has credit.
    grant(&service, seed.org_id, 500_000, "seed").await;
    service
        .check_or_fail_for_user(seed.user_id, seed.org_id)
        .await
        .expect("RLS gate passes with credit");
}

#[test]
fn out_of_credit_maps_to_402_and_exceeded_to_429() {
    let out = HttpError::Billing(BillingError::OutOfCredit {
        org: OrgId::new(),
        balance_micro_usd: 0,
    });
    assert_eq!(out.into_response().status().as_u16(), 402);

    let over_cap = HttpError::Billing(BillingError::Exceeded {
        org: OrgId::new(),
        used_micro_usd: 10,
        cap_micro_usd: 5,
    });
    assert_eq!(over_cap.into_response().status().as_u16(), 429);
}
