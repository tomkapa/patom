//! Postgres-backed [`SubscriptionStore`] over the `cloud` schema.
//!
//! All access is privileged (the webhook has no user principal; the
//! entitlement read is a system lookup keyed by an already-authenticated
//! `org_id`) — tenant scoping is the explicit `org_id` bound on every
//! statement, mirroring core's seeder paths. Wall-clock comes from the
//! injected [`SharedClock`], never `NOW()` (CLAUDE.md §11).

use std::fmt;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use patom::auth::{OrgId, run_privileged};
use patom::clock::SharedClock;
use sqlx::PgPool;

use super::error::LemonSqueezyError;
use super::store::{NewSubscription, SubscriptionRecord, SubscriptionStore};
use super::types::{
    LsCustomerId, LsEventId, LsSubscriptionId, LsVariantId, Plan, SubscriptionId,
    SubscriptionStatus,
};

/// Single source of truth for the subscription SELECT shape.
const SUBSCRIPTION_SELECT: &str = "id, org_id, ls_customer_id, ls_subscription_id, \
    ls_variant_id, plan, status, current_period_end, created_at, updated_at \
    FROM cloud.subscriptions";

pub struct PgSubscriptionStore {
    pool: PgPool,
    clock: SharedClock,
}

impl PgSubscriptionStore {
    #[must_use]
    pub fn new(pool: PgPool, clock: SharedClock) -> Self {
        Self { pool, clock }
    }
}

impl fmt::Debug for PgSubscriptionStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PgSubscriptionStore")
            .finish_non_exhaustive()
    }
}

/// Raw row mapped 1:1 from `cloud.subscriptions`; the typed [`SubscriptionRecord`]
/// is produced via [`TryFrom`] at the boundary (CLAUDE.md §1).
#[derive(sqlx::FromRow)]
struct SubscriptionRow {
    id: SubscriptionId,
    org_id: OrgId,
    ls_customer_id: Option<String>,
    ls_subscription_id: String,
    ls_variant_id: Option<String>,
    plan: Plan,
    status: SubscriptionStatus,
    current_period_end: Option<DateTime<Utc>>,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

impl TryFrom<SubscriptionRow> for SubscriptionRecord {
    type Error = LemonSqueezyError;
    fn try_from(row: SubscriptionRow) -> Result<Self, Self::Error> {
        Ok(Self {
            id: row.id,
            org_id: row.org_id,
            ls_customer_id: row.ls_customer_id.map(LsCustomerId::try_from).transpose()?,
            ls_subscription_id: LsSubscriptionId::try_from(row.ls_subscription_id)?,
            ls_variant_id: row.ls_variant_id.map(LsVariantId::try_from).transpose()?,
            plan: row.plan,
            status: row.status,
            current_period_end: row.current_period_end,
            created_at: row.created_at,
            updated_at: row.updated_at,
        })
    }
}

#[async_trait]
impl SubscriptionStore for PgSubscriptionStore {
    async fn upsert(&self, sub: NewSubscription) -> Result<(), LemonSqueezyError> {
        let now = self.clock.now_utc();
        let id = SubscriptionId::new();
        run_privileged(&self.pool, async |tx| {
            // Insert keyed by the natural key; on redelivery / status change the
            // existing row is updated in place (its `id` and `created_at` are
            // preserved). Static SQL, bound params only (§10).
            sqlx::query(
                "INSERT INTO cloud.subscriptions \
                     (id, org_id, ls_customer_id, ls_subscription_id, ls_variant_id, \
                      plan, status, current_period_end, created_at, updated_at) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $9) \
                 ON CONFLICT (ls_subscription_id) DO UPDATE SET \
                     org_id = EXCLUDED.org_id, \
                     ls_customer_id = EXCLUDED.ls_customer_id, \
                     ls_variant_id = EXCLUDED.ls_variant_id, \
                     plan = EXCLUDED.plan, \
                     status = EXCLUDED.status, \
                     current_period_end = EXCLUDED.current_period_end, \
                     updated_at = EXCLUDED.updated_at",
            )
            .bind(id)
            .bind(sub.org_id)
            .bind(sub.ls_customer_id.as_ref().map(LsCustomerId::as_str))
            .bind(sub.ls_subscription_id.as_str())
            .bind(sub.ls_variant_id.as_ref().map(LsVariantId::as_str))
            .bind(sub.plan)
            .bind(sub.status)
            .bind(sub.current_period_end)
            .bind(now)
            .execute(&mut **tx)
            .await?;
            Ok(())
        })
        .await
    }

    async fn read_for_org(
        &self,
        org: OrgId,
    ) -> Result<Option<SubscriptionRecord>, LemonSqueezyError> {
        let sql = format!(
            "SELECT {SUBSCRIPTION_SELECT} WHERE org_id = $1 ORDER BY updated_at DESC LIMIT 1"
        );
        let row =
            run_privileged::<Option<SubscriptionRow>, LemonSqueezyError>(&self.pool, async |tx| {
                Ok(sqlx::query_as::<_, SubscriptionRow>(&sql)
                    .bind(org)
                    .fetch_optional(&mut **tx)
                    .await?)
            })
            .await?;
        row.map(SubscriptionRecord::try_from).transpose()
    }

    async fn record_event_once(
        &self,
        event_id: &LsEventId,
        org: Option<OrgId>,
    ) -> Result<bool, LemonSqueezyError> {
        let now = self.clock.now_utc();
        let inserted = run_privileged::<u64, LemonSqueezyError>(&self.pool, async |tx| {
            // `DO NOTHING` makes the ledger insert idempotent: a redelivery
            // affects zero rows, telling the caller it was already applied.
            let result = sqlx::query(
                "INSERT INTO cloud.webhook_events (ls_event_id, org_id, received_at) \
                 VALUES ($1, $2, $3) ON CONFLICT (ls_event_id) DO NOTHING",
            )
            .bind(event_id.as_str())
            .bind(org)
            .bind(now)
            .execute(&mut **tx)
            .await?;
            Ok(result.rows_affected())
        })
        .await?;
        Ok(inserted == 1)
    }

    async fn list_stale(
        &self,
        cutoff: DateTime<Utc>,
        limit: i64,
    ) -> Result<Vec<SubscriptionRecord>, LemonSqueezyError> {
        let sql = format!(
            "SELECT {SUBSCRIPTION_SELECT} WHERE updated_at < $1 ORDER BY updated_at ASC LIMIT $2"
        );
        let rows =
            run_privileged::<Vec<SubscriptionRow>, LemonSqueezyError>(&self.pool, async |tx| {
                Ok(sqlx::query_as::<_, SubscriptionRow>(&sql)
                    .bind(cutoff)
                    .bind(limit)
                    .fetch_all(&mut **tx)
                    .await?)
            })
            .await?;
        rows.into_iter().map(SubscriptionRecord::try_from).collect()
    }
}
