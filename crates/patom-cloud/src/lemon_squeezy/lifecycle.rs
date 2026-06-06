//! Maps a verified webhook event to a subscription-store action.
//!
//! Only `subscription_*` (non-payment) events carry the full subscription
//! state we persist; `order_created` and `subscription_payment_*` are acked
//! without a state write — the resulting status change always arrives as an
//! accompanying `subscription_updated`, and the reconciliation poll is the net
//! for anything missed. Non-DB problems (no org mapping, unknown variant /
//! status) are logged and acked (returning `Ok`), never retried; only a real
//! storage failure propagates so Lemon Squeezy retries.

use patom::auth::OrgId;
use tracing::{debug, info, warn};

use super::deps::CloudDeps;
use super::error::LemonSqueezyError;
use super::payload::WebhookEnvelope;
use super::store::NewSubscription;
use super::types::{LsCustomerId, LsSubscriptionId, LsVariantId, SubscriptionStatus};

/// `subscription_*` but not `subscription_payment_*`.
fn carries_subscription_state(event_name: &str) -> bool {
    event_name.starts_with("subscription_") && !event_name.starts_with("subscription_payment_")
}

/// Apply a verified event. `org` is the mapping resolved from
/// `meta.custom_data.org_id` by the caller.
///
/// # Errors
/// [`LemonSqueezyError::Db`] on a storage failure (the only retriable case).
pub async fn apply(
    deps: &CloudDeps,
    env: &WebhookEnvelope,
    org: Option<OrgId>,
) -> Result<(), LemonSqueezyError> {
    let event = env.meta.event_name.as_str();
    if !carries_subscription_state(event) {
        debug!(event = "lemon_squeezy.webhook.ignored", name = event);
        return Ok(());
    }
    let Some(org) = org else {
        warn!(event = "lemon_squeezy.webhook.missing_org", name = event);
        return Ok(());
    };
    let attrs = &env.data.attributes;
    let (Some(sub_id), Some(variant), Some(status_raw)) = (
        env.data.id.as_deref(),
        attrs.variant_id,
        attrs.status.as_deref(),
    ) else {
        warn!(event = "lemon_squeezy.webhook.incomplete", name = event);
        return Ok(());
    };
    let ls_variant_id = LsVariantId::try_from(variant.to_string())?;
    let Some(plan) = deps.config.plan_for(&ls_variant_id) else {
        warn!(
            event = "lemon_squeezy.webhook.unmapped_variant",
            variant = variant
        );
        return Ok(());
    };
    let Some(status) = SubscriptionStatus::parse(status_raw) else {
        warn!(
            event = "lemon_squeezy.webhook.unknown_status",
            status = status_raw
        );
        return Ok(());
    };
    let ls_customer_id = attrs
        .customer_id
        .map(|c| LsCustomerId::try_from(c.to_string()))
        .transpose()?;
    deps.subscriptions
        .upsert(NewSubscription {
            org_id: org,
            ls_customer_id,
            ls_subscription_id: LsSubscriptionId::try_from(sub_id.to_owned())?,
            ls_variant_id: Some(ls_variant_id),
            plan,
            status,
            // Active subs renew; cancelled ones have an end date.
            current_period_end: attrs.renews_at.or(attrs.ends_at),
        })
        .await?;
    info!(
        event = "lemon_squeezy.webhook.subscription_upserted",
        name = event,
        patom.org.id = %org,
        plan = plan.as_str(),
        status = status.as_str(),
    );
    Ok(())
}
