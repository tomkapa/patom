//! Billing-backed [`Entitlements`] — the paid answer to core's seam (#131).
//!
//! Resolves an org's agent cap from its `cloud.subscriptions` row. Monetization
//! scales on **agent count, not features**, so [`Entitlements::allows`] is
//! always `true`; only [`Entitlements::agent_limit`] carries the policy. Cloud
//! has no perpetual free tier: an org with no active subscription gets
//! `Max(0)`.

use async_trait::async_trait;
use patom::auth::OrgId;
use patom::clock::SharedClock;
use patom::{AgentLimit, Entitlements, Feature};
use tracing::error;

use super::limits::PAST_DUE_GRACE_DAYS;
use super::store::{SharedSubscriptionStore, SubscriptionRecord};
use super::types::{Plan, SubscriptionStatus};

/// Agent cap per launch-pricing plan.
fn plan_cap(plan: Plan) -> AgentLimit {
    match plan {
        Plan::Starter => AgentLimit::Max(3),
        Plan::Growth => AgentLimit::Max(10),
        Plan::Scale => AgentLimit::Max(30),
        Plan::Enterprise => AgentLimit::Unlimited,
    }
}

/// The cloud's billing-backed entitlement policy.
#[derive(Debug)]
pub struct BillingEntitlements {
    subscriptions: SharedSubscriptionStore,
    clock: SharedClock,
}

impl BillingEntitlements {
    #[must_use]
    pub fn new(subscriptions: SharedSubscriptionStore, clock: SharedClock) -> Self {
        Self {
            subscriptions,
            clock,
        }
    }

    /// Cap a subscription resolves to, factoring in status + grace.
    fn cap_for(&self, sub: &SubscriptionRecord) -> AgentLimit {
        match sub.status {
            // Paid and current (a trial is the full product).
            SubscriptionStatus::Active | SubscriptionStatus::OnTrial => plan_cap(sub.plan),
            // Failed renewal: keep the paid cap through the dunning grace, then
            // downgrade.
            SubscriptionStatus::PastDue if self.within_grace(sub) => plan_cap(sub.plan),
            // Past grace, or otherwise inactive → no-subscription cap.
            SubscriptionStatus::PastDue
            | SubscriptionStatus::Paused
            | SubscriptionStatus::Unpaid
            | SubscriptionStatus::Cancelled
            | SubscriptionStatus::Expired => AgentLimit::Max(0),
        }
    }

    /// Whether `now <= current_period_end + grace`. No period end → no grace.
    fn within_grace(&self, sub: &SubscriptionRecord) -> bool {
        sub.current_period_end.is_some_and(|end| {
            self.clock.now_utc() <= end + chrono::Duration::days(PAST_DUE_GRACE_DAYS)
        })
    }
}

#[async_trait]
impl Entitlements for BillingEntitlements {
    async fn agent_limit(&self, org: OrgId) -> AgentLimit {
        match self.subscriptions.read_for_org(org).await {
            Ok(Some(sub)) => self.cap_for(&sub),
            // No subscription on Cloud → must subscribe before creating agents.
            Ok(None) => AgentLimit::Max(0),
            Err(e) => {
                // Fail closed: a storage error here means the shared pool is in
                // trouble, so the create would fail anyway; surface 402 over a
                // wrong unlimited grant.
                error!(error = ?e, event = "billing.agent_limit.read_failed", patom.org.id = %org);
                AgentLimit::Max(0)
            }
        }
    }

    async fn allows(&self, _org: OrgId, _feature: Feature) -> bool {
        // Every feature is on for every tier; only agent count is gated.
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Duration, Utc};
    use patom::clock::SystemClock;
    use sqlx::PgPool;
    use std::sync::Arc;

    use crate::lemon_squeezy::pg_store::PgSubscriptionStore;
    use crate::lemon_squeezy::store::NewSubscription;
    use crate::lemon_squeezy::types::{LsSubscriptionId, LsVariantId};

    async fn entitlements(pool: &PgPool) -> (BillingEntitlements, SharedSubscriptionStore) {
        crate::run_migrations(pool).await.expect("cloud migrations");
        let store: SharedSubscriptionStore = Arc::new(PgSubscriptionStore::new(
            pool.clone(),
            SystemClock::shared(),
        ));
        (
            BillingEntitlements::new(store.clone(), SystemClock::shared()),
            store,
        )
    }

    fn sub(
        org: OrgId,
        plan: Plan,
        status: SubscriptionStatus,
        period_end: Option<chrono::DateTime<Utc>>,
    ) -> NewSubscription {
        NewSubscription {
            org_id: org,
            ls_customer_id: None,
            ls_subscription_id: LsSubscriptionId::try_from(format!("sub_{org}")).expect("id"),
            ls_variant_id: Some(LsVariantId::try_from("v1").expect("variant")),
            plan,
            status,
            current_period_end: period_end,
        }
    }

    #[sqlx::test(migrations = "../patom-core/migrations")]
    async fn no_subscription_is_capped_to_zero(pool: PgPool) {
        let (ent, _store) = entitlements(&pool).await;
        assert_eq!(ent.agent_limit(OrgId::new()).await, AgentLimit::Max(0));
    }

    #[sqlx::test(migrations = "../patom-core/migrations")]
    async fn active_plans_map_to_their_caps(pool: PgPool) {
        let (ent, store) = entitlements(&pool).await;
        for (plan, expected) in [
            (Plan::Starter, AgentLimit::Max(3)),
            (Plan::Growth, AgentLimit::Max(10)),
            (Plan::Scale, AgentLimit::Max(30)),
            (Plan::Enterprise, AgentLimit::Unlimited),
        ] {
            let org = OrgId::new();
            store
                .upsert(sub(org, plan, SubscriptionStatus::Active, None))
                .await
                .expect("upsert");
            assert_eq!(ent.agent_limit(org).await, expected, "plan {plan:?}");
        }
    }

    #[sqlx::test(migrations = "../patom-core/migrations")]
    async fn past_due_within_grace_keeps_the_paid_cap(pool: PgPool) {
        let (ent, store) = entitlements(&pool).await;
        let org = OrgId::new();
        // Period ended yesterday; grace is 3 days → still covered.
        let end = Utc::now() - Duration::days(1);
        store
            .upsert(sub(
                org,
                Plan::Growth,
                SubscriptionStatus::PastDue,
                Some(end),
            ))
            .await
            .expect("upsert");
        assert_eq!(ent.agent_limit(org).await, AgentLimit::Max(10));
    }

    #[sqlx::test(migrations = "../patom-core/migrations")]
    async fn past_due_beyond_grace_downgrades(pool: PgPool) {
        let (ent, store) = entitlements(&pool).await;
        let org = OrgId::new();
        // Period ended 4 days ago; past the 3-day grace → downgrade.
        let end = Utc::now() - Duration::days(4);
        store
            .upsert(sub(
                org,
                Plan::Scale,
                SubscriptionStatus::PastDue,
                Some(end),
            ))
            .await
            .expect("upsert");
        assert_eq!(ent.agent_limit(org).await, AgentLimit::Max(0));
    }

    #[sqlx::test(migrations = "../patom-core/migrations")]
    async fn cancelled_downgrades_to_zero(pool: PgPool) {
        let (ent, store) = entitlements(&pool).await;
        let org = OrgId::new();
        store
            .upsert(sub(org, Plan::Scale, SubscriptionStatus::Cancelled, None))
            .await
            .expect("upsert");
        assert_eq!(ent.agent_limit(org).await, AgentLimit::Max(0));
    }
}
