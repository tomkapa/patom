//! Deserialization shapes for Lemon Squeezy webhook payloads.
//!
//! Only the fields we act on are modelled; everything else is ignored. Note
//! Lemon Squeezy sends `customer_id` / `variant_id` as JSON **numbers** and the
//! subscription id as `data.id` (a string); `custom_data` carries the strings
//! we set at checkout (notably `org_id`).

use chrono::{DateTime, Utc};
use serde::Deserialize;

/// Top-level webhook envelope.
#[derive(Debug, Deserialize)]
pub struct WebhookEnvelope {
    pub meta: Meta,
    pub data: Data,
}

#[derive(Debug, Deserialize)]
pub struct Meta {
    /// e.g. `subscription_created`, `subscription_updated`,
    /// `subscription_payment_success`.
    pub event_name: String,
    #[serde(default)]
    pub custom_data: Option<CustomData>,
}

/// Checkout-time custom data echoed back on every event for the subscription.
#[derive(Debug, Deserialize)]
pub struct CustomData {
    /// The Patom org this subscription belongs to. Set when the checkout is
    /// created; the only way the webhook can attribute the subscription.
    #[serde(default)]
    pub org_id: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct Data {
    /// For `subscription_*` events this is the Lemon Squeezy subscription id.
    #[serde(default)]
    pub id: Option<String>,
    pub attributes: Attributes,
}

#[derive(Debug, Deserialize)]
pub struct Attributes {
    #[serde(default)]
    pub customer_id: Option<i64>,
    #[serde(default)]
    pub variant_id: Option<i64>,
    #[serde(default)]
    pub status: Option<String>,
    /// Next renewal (the current paid-period end for an active subscription).
    #[serde(default)]
    pub renews_at: Option<DateTime<Utc>>,
    /// When access ends for a cancelled subscription.
    #[serde(default)]
    pub ends_at: Option<DateTime<Utc>>,
}
