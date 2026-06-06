//! Storage trait + read/write models for `cloud.subscriptions` and the webhook
//! idempotency ledger.

use std::fmt;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use patom::auth::OrgId;

use super::error::LemonSqueezyError;
use super::types::{
    LsCustomerId, LsEventId, LsSubscriptionId, LsVariantId, Plan, SubscriptionId,
    SubscriptionStatus,
};

/// A persisted subscription row (read model). No card data — Lemon Squeezy (the
/// Merchant of Record) owns it; we keep only ids + status + period.
#[derive(Debug, Clone)]
pub struct SubscriptionRecord {
    pub id: SubscriptionId,
    pub org_id: OrgId,
    pub ls_customer_id: Option<LsCustomerId>,
    pub ls_subscription_id: LsSubscriptionId,
    pub ls_variant_id: Option<LsVariantId>,
    pub plan: Plan,
    pub status: SubscriptionStatus,
    pub current_period_end: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Upsert payload minted from a webhook event. `id` / timestamps are managed by
/// the store; the natural key is [`Self::ls_subscription_id`].
#[derive(Debug, Clone)]
pub struct NewSubscription {
    pub org_id: OrgId,
    pub ls_customer_id: Option<LsCustomerId>,
    pub ls_subscription_id: LsSubscriptionId,
    pub ls_variant_id: Option<LsVariantId>,
    pub plan: Plan,
    pub status: SubscriptionStatus,
    pub current_period_end: Option<DateTime<Utc>>,
}

/// Persistence for billing subscriptions + the webhook idempotency ledger.
#[async_trait]
pub trait SubscriptionStore: fmt::Debug + Send + Sync {
    /// Insert or update the subscription keyed by `ls_subscription_id`. Driven
    /// by `subscription_*` webhooks.
    ///
    /// # Errors
    /// [`LemonSqueezyError::Db`] on a storage failure.
    async fn upsert(&self, sub: NewSubscription) -> Result<(), LemonSqueezyError>;

    /// The org's current subscription (most-recently-updated row), if any.
    /// Drives the entitlement gate.
    ///
    /// # Errors
    /// [`LemonSqueezyError::Db`] on a storage failure.
    async fn read_for_org(
        &self,
        org: OrgId,
    ) -> Result<Option<SubscriptionRecord>, LemonSqueezyError>;

    /// Record a webhook event id, exactly once. Returns `true` if this call
    /// recorded it (the caller should process the event) and `false` if it was
    /// already present (a redelivery — skip, already applied).
    ///
    /// # Errors
    /// [`LemonSqueezyError::Db`] on a storage failure.
    async fn record_event_once(
        &self,
        event_id: &LsEventId,
        org: Option<OrgId>,
    ) -> Result<bool, LemonSqueezyError>;
}
