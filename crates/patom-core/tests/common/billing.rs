//! Shared credit-billing test plumbing: a configurable entitlement policy and
//! the small grant/read helpers the `credit_*` suites all need.

use std::sync::Arc;

use patom::auth::OrgId;
use patom::billing::{BillingService, CostMicros, GrantAmount, LedgerReason, PgBillingService};
use patom::clock::SystemClock;
use patom::entitlements::{AgentLimit, Entitlements, Feature, SharedEntitlements};
use patom::runtime::IdempotencyKey;
use sqlx::PgPool;

/// Cloud-shaped entitlement policy: the credit gate is active and a signup
/// grant of `signup_micros` fires (`None` = no signup grant). Stands in for
/// `patom_cloud::CloudEntitlements` so the credit paths can be tested without
/// the cloud feature linked.
#[derive(Debug)]
pub struct TestCreditPolicy {
    pub signup_micros: Option<i64>,
}

impl Entitlements for TestCreditPolicy {
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
        self.signup_micros
            .and_then(|m| GrantAmount::try_from(m).ok())
    }
}

/// Credit gate active, no signup grant (the cloud-without-promo shape).
pub fn active_credit_policy() -> SharedEntitlements {
    Arc::new(TestCreditPolicy {
        signup_micros: None,
    })
}

/// Credit gate active plus a signup grant of `micros`.
pub fn signup_grant_policy(micros: i64) -> SharedEntitlements {
    Arc::new(TestCreditPolicy {
        signup_micros: Some(micros),
    })
}

/// A billing service whose credit gate is active (system clock).
pub fn active_service(pool: &PgPool) -> PgBillingService {
    PgBillingService::with_entitlements(pool.clone(), SystemClock::shared(), active_credit_policy())
}

pub fn cost(micros: i64) -> CostMicros {
    CostMicros::try_from(micros).expect("non-negative cost")
}

pub fn grant_amount(micros: i64) -> GrantAmount {
    GrantAmount::try_from(micros).expect("positive grant")
}

pub fn idem_key(s: &str) -> IdempotencyKey {
    IdempotencyKey::try_from(s.to_owned()).expect("valid idempotency key")
}

/// Grant `micros` to `org` with a `Manual` reason, keyed by `key`.
pub async fn grant(service: &PgBillingService, org: OrgId, micros: i64, key: &str) {
    service
        .grant_credit(
            org,
            grant_amount(micros),
            LedgerReason::Manual,
            &idem_key(key),
            None,
        )
        .await
        .expect("grant");
}

/// The materialized credit totals `(balance, granted, used)` for `org`,
/// RLS-bypassing via the owner pool. `None` when the org has no `org_credits` row.
pub async fn read_org_credits(pool: &PgPool, org: OrgId) -> Option<(i64, i64, i64)> {
    sqlx::query_as(
        "SELECT balance_micro_usd, granted_total_micro_usd, used_total_micro_usd \
         FROM org_credits WHERE org_id = $1",
    )
    .bind(org)
    .fetch_optional(pool)
    .await
    .expect("read org_credits")
}

/// The `usage` debit deltas recorded for `org`, oldest first.
pub async fn usage_entries(pool: &PgPool, org: OrgId) -> Vec<i64> {
    sqlx::query_scalar(
        "SELECT delta_micro_usd FROM org_credit_ledger \
         WHERE org_id = $1 AND kind = 'debit' AND reason = 'usage' ORDER BY created_at",
    )
    .bind(org)
    .fetch_all(pool)
    .await
    .expect("read usage entries")
}
