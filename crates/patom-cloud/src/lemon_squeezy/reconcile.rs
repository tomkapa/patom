//! Reconciliation poll — the safety net for webhooks we never received (#131).
//!
//! Webhooks are the primary path; if one is dropped, a subscription's stored
//! state drifts from Lemon Squeezy's. A periodic sweep re-fetches the state of
//! stale subscriptions and upserts the truth. Bounded per tick (`RECONCILE_BATCH`)
//! and cancel-aware via the shared [`ScheduledTask`] primitive.

use chrono::Duration;
use patom::clock::SharedClock;
use patom::scheduling::ScheduledTask;
use tokio_util::sync::CancellationToken;
use tracing::info;

use super::client::SharedCheckoutClient;
use super::config::LemonSqueezyConfig;
use super::error::LemonSqueezyError;
use super::limits::{RECONCILE_BATCH, RECONCILE_INTERVAL, RECONCILE_STALE_AFTER_SECS};
use super::store::{NewSubscription, SharedSubscriptionStore};

/// Handles the reconcile poll needs, cloned into the spawned task.
#[derive(Debug, Clone)]
pub struct ReconcileDeps {
    pub client: SharedCheckoutClient,
    pub subscriptions: SharedSubscriptionStore,
    pub config: LemonSqueezyConfig,
    pub clock: SharedClock,
}

/// Sweep once: refresh every subscription not updated since `cutoff` (capped at
/// `limit`) from the Lemon Squeezy API, upserting the truth. Returns how many
/// were refreshed.
///
/// # Errors
/// [`LemonSqueezyError`] from the store or the API; a failing sweep is logged
/// by the scheduler and retried next tick.
pub async fn reconcile_once(
    client: &dyn super::client::LsCheckoutClient,
    store: &dyn super::store::SubscriptionStore,
    config: &LemonSqueezyConfig,
    cutoff: chrono::DateTime<chrono::Utc>,
    limit: i64,
) -> Result<usize, LemonSqueezyError> {
    let stale = store.list_stale(cutoff, limit).await?;
    let mut refreshed = 0;
    for sub in stale {
        let remote = client.get_subscription(&sub.ls_subscription_id).await?;
        // Keep the existing plan/variant when the remote variant isn't in the
        // configured map (or absent) — only status/period are authoritative
        // here.
        let plan = remote
            .variant_id
            .as_ref()
            .and_then(|v| config.plan_for(v))
            .unwrap_or(sub.plan);
        store
            .upsert(NewSubscription {
                org_id: sub.org_id,
                ls_customer_id: remote.customer_id.or(sub.ls_customer_id),
                ls_subscription_id: sub.ls_subscription_id,
                ls_variant_id: remote.variant_id.or(sub.ls_variant_id),
                plan,
                status: remote.status,
                current_period_end: remote.current_period_end,
            })
            .await?;
        refreshed += 1;
    }
    Ok(refreshed)
}

/// Spawn the reconciliation poll. The returned [`ScheduledTask`] ticks every
/// [`RECONCILE_INTERVAL`], stops on `cancel`, and is joined by `run_server` on
/// shutdown (so it never floats, §7).
#[must_use]
pub fn spawn(deps: ReconcileDeps, cancel: CancellationToken) -> ScheduledTask {
    ScheduledTask::spawn(
        "billing_reconcile",
        RECONCILE_INTERVAL,
        Some(cancel),
        move || {
            let deps = deps.clone();
            async move {
                let cutoff = deps.clock.now_utc() - Duration::seconds(RECONCILE_STALE_AFTER_SECS);
                let n = reconcile_once(
                    deps.client.as_ref(),
                    deps.subscriptions.as_ref(),
                    &deps.config,
                    cutoff,
                    RECONCILE_BATCH,
                )
                .await?;
                if n > 0 {
                    info!(event = "lemon_squeezy.reconcile.swept", refreshed = n);
                }
                Ok::<(), LemonSqueezyError>(())
            }
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    use async_trait::async_trait;
    use chrono::Utc;
    use patom::auth::OrgId;
    use patom::clock::SystemClock;
    use sqlx::PgPool;

    use super::super::client::{CheckoutCreate, LsCheckoutClient, RemoteSubscription};
    use super::super::pg_store::PgSubscriptionStore;
    use super::super::store::SubscriptionStore;
    use super::super::types::{
        LsCustomerId, LsSubscriptionId, LsVariantId, Plan, SubscriptionStatus,
    };
    use std::collections::HashMap;

    /// Returns a fixed remote state for any subscription; records the ids asked.
    #[derive(Debug)]
    struct FakeClient {
        remote: RemoteSubscription,
        asked: Mutex<Vec<String>>,
    }

    #[async_trait]
    impl LsCheckoutClient for FakeClient {
        async fn create_checkout(&self, _req: CheckoutCreate) -> Result<String, LemonSqueezyError> {
            Ok(String::new())
        }
        async fn get_subscription(
            &self,
            id: &LsSubscriptionId,
        ) -> Result<RemoteSubscription, LemonSqueezyError> {
            self.asked
                .lock()
                .expect("lock")
                .push(id.as_str().to_owned());
            Ok(self.remote.clone())
        }
    }

    fn config() -> LemonSqueezyConfig {
        LemonSqueezyConfig::new(
            patom::types::SecretString::try_from("s".to_string()).expect("secret"),
            patom::types::SecretString::try_from("a".to_string()).expect("api"),
            "store".to_string(),
            HashMap::new(),
        )
    }

    #[sqlx::test(migrations = "../patom-core/migrations")]
    async fn reconcile_refreshes_drifted_status(pool: PgPool) {
        crate::run_migrations(&pool).await.expect("migrate");
        let store = PgSubscriptionStore::new(pool.clone(), SystemClock::shared());
        let org = OrgId::new();
        // Stored state says active; the real state (per the API) is cancelled —
        // the subscription_cancelled webhook was missed.
        store
            .upsert(NewSubscription {
                org_id: org,
                ls_customer_id: None,
                ls_subscription_id: LsSubscriptionId::try_from("sub_1").expect("id"),
                ls_variant_id: Some(LsVariantId::try_from("v1").expect("variant")),
                plan: Plan::Scale,
                status: SubscriptionStatus::Active,
                current_period_end: None,
            })
            .await
            .expect("seed");

        let client = FakeClient {
            remote: RemoteSubscription {
                variant_id: Some(LsVariantId::try_from("v1").expect("variant")),
                customer_id: Some(LsCustomerId::try_from("cus_1").expect("cus")),
                status: SubscriptionStatus::Cancelled,
                current_period_end: None,
            },
            asked: Mutex::new(Vec::new()),
        };

        // Cutoff in the future → everything counts as stale.
        let cutoff = Utc::now() + Duration::hours(1);
        let n = reconcile_once(&client, &store, &config(), cutoff, 100)
            .await
            .expect("reconcile");
        assert_eq!(n, 1);
        assert_eq!(client.asked.lock().expect("lock").as_slice(), ["sub_1"]);

        let got = store.read_for_org(org).await.expect("read").expect("some");
        assert_eq!(got.status, SubscriptionStatus::Cancelled, "drift corrected");
    }

    #[sqlx::test(migrations = "../patom-core/migrations")]
    async fn reconcile_skips_fresh_subscriptions(pool: PgPool) {
        crate::run_migrations(&pool).await.expect("migrate");
        let store = PgSubscriptionStore::new(pool.clone(), SystemClock::shared());
        store
            .upsert(NewSubscription {
                org_id: OrgId::new(),
                ls_customer_id: None,
                ls_subscription_id: LsSubscriptionId::try_from("sub_fresh").expect("id"),
                ls_variant_id: None,
                plan: Plan::Starter,
                status: SubscriptionStatus::Active,
                current_period_end: None,
            })
            .await
            .expect("seed");

        let client = FakeClient {
            remote: RemoteSubscription {
                variant_id: None,
                customer_id: None,
                status: SubscriptionStatus::Active,
                current_period_end: None,
            },
            asked: Mutex::new(Vec::new()),
        };

        // Cutoff in the past → the just-written row is NOT stale.
        let cutoff = Utc::now() - Duration::hours(1);
        let n = reconcile_once(&client, &store, &config(), cutoff, 100)
            .await
            .expect("reconcile");
        assert_eq!(n, 0, "fresh subscriptions are not refetched");
        assert!(client.asked.lock().expect("lock").is_empty());
    }
}
