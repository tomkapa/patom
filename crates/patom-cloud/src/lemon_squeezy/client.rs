//! Lemon Squeezy REST API client (checkout creation; reconciliation later).
//!
//! Behind a trait so the checkout handler can be tested with a fake and the
//! real impl never runs in unit tests (CLAUDE.md §3 — mock paid external
//! services). The HTTP impl reuses the shared `reqwest::Client` and bounds
//! every call with a timeout (§5).

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use patom::auth::OrgId;
use patom::types::SecretString;
use reqwest::Client;
use serde::Deserialize;

use super::error::LemonSqueezyError;
use super::limits::LS_API_TIMEOUT;
use super::types::{LsCustomerId, LsSubscriptionId, LsVariantId, SubscriptionStatus};

/// Default Lemon Squeezy API base. Overridable (sandbox / tests) at construction.
pub const LEMON_SQUEEZY_API_BASE: &str = "https://api.lemonsqueezy.com";

/// Inputs to create a hosted checkout for one variant.
#[derive(Debug, Clone)]
pub struct CheckoutCreate {
    pub store_id: String,
    pub variant: LsVariantId,
    /// Org to attribute the resulting subscription to — echoed back on every
    /// webhook as `meta.custom_data.org_id`.
    pub org_id: OrgId,
    /// Where Lemon Squeezy returns the buyer after payment. `None` uses the
    /// store's default.
    pub redirect_url: Option<String>,
}

/// A subscription's current state as read back from the Lemon Squeezy API —
/// the fields reconciliation refreshes when a webhook was missed.
#[derive(Debug, Clone)]
pub struct RemoteSubscription {
    pub variant_id: Option<LsVariantId>,
    pub customer_id: Option<LsCustomerId>,
    pub status: SubscriptionStatus,
    pub current_period_end: Option<DateTime<Utc>>,
}

/// The Lemon Squeezy REST API (checkout creation + subscription read).
#[async_trait]
pub trait LsCheckoutClient: std::fmt::Debug + Send + Sync {
    /// Create a hosted checkout and return its URL.
    ///
    /// # Errors
    /// [`LemonSqueezyError::Http`] on transport failure or
    /// [`LemonSqueezyError::Upstream`] on a non-success status.
    async fn create_checkout(&self, req: CheckoutCreate) -> Result<String, LemonSqueezyError>;

    /// Fetch a subscription's current state (reconciliation backfill).
    ///
    /// # Errors
    /// [`LemonSqueezyError::Http`] / [`LemonSqueezyError::Upstream`] on the API
    /// call, or [`LemonSqueezyError::Parse`] if the returned status is unknown.
    async fn get_subscription(
        &self,
        id: &LsSubscriptionId,
    ) -> Result<RemoteSubscription, LemonSqueezyError>;
}

/// Cheap-clone handle to a checkout client.
pub type SharedCheckoutClient = std::sync::Arc<dyn LsCheckoutClient>;

/// reqwest-backed [`LsCheckoutClient`].
pub struct HttpLemonSqueezyClient {
    http: Client,
    api_key: SecretString,
    base_url: String,
}

impl HttpLemonSqueezyClient {
    #[must_use]
    pub fn new(http: Client, api_key: SecretString, base_url: String) -> Self {
        Self {
            http,
            api_key,
            base_url,
        }
    }
}

impl std::fmt::Debug for HttpLemonSqueezyClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HttpLemonSqueezyClient")
            .field("base_url", &self.base_url)
            .finish_non_exhaustive()
    }
}

/// Minimal slice of the JSON:API checkout response we read.
#[derive(Deserialize)]
struct CheckoutResponse {
    data: CheckoutResponseData,
}
#[derive(Deserialize)]
struct CheckoutResponseData {
    attributes: CheckoutResponseAttrs,
}
#[derive(Deserialize)]
struct CheckoutResponseAttrs {
    url: String,
}

#[async_trait]
impl LsCheckoutClient for HttpLemonSqueezyClient {
    async fn create_checkout(&self, req: CheckoutCreate) -> Result<String, LemonSqueezyError> {
        // JSON:API body: attach the org via checkout custom data so it returns
        // on every webhook, and link the store + variant relationships.
        let mut attributes = serde_json::json!({
            "checkout_data": { "custom": { "org_id": req.org_id.as_uuid().to_string() } }
        });
        if let Some(redirect) = req.redirect_url {
            attributes["product_options"] = serde_json::json!({ "redirect_url": redirect });
        }
        let body = serde_json::json!({
            "data": {
                "type": "checkouts",
                "attributes": attributes,
                "relationships": {
                    "store": { "data": { "type": "stores", "id": req.store_id } },
                    "variant": { "data": { "type": "variants", "id": req.variant.as_str() } }
                }
            }
        });

        let response = self
            .http
            .post(format!("{}/v1/checkouts", self.base_url))
            .header("Authorization", format!("Bearer {}", self.api_key.expose()))
            .header("Accept", "application/vnd.api+json")
            .header("Content-Type", "application/vnd.api+json")
            .timeout(LS_API_TIMEOUT)
            .json(&body)
            .send()
            .await?;

        let status = response.status();
        if !status.is_success() {
            return Err(LemonSqueezyError::Upstream {
                status: status.as_u16(),
            });
        }
        let parsed: CheckoutResponse = response.json().await?;
        Ok(parsed.data.attributes.url)
    }

    async fn get_subscription(
        &self,
        id: &LsSubscriptionId,
    ) -> Result<RemoteSubscription, LemonSqueezyError> {
        let response = self
            .http
            .get(format!(
                "{}/v1/subscriptions/{}",
                self.base_url,
                id.as_str()
            ))
            .header("Authorization", format!("Bearer {}", self.api_key.expose()))
            .header("Accept", "application/vnd.api+json")
            .timeout(LS_API_TIMEOUT)
            .send()
            .await?;

        let status = response.status();
        if !status.is_success() {
            return Err(LemonSqueezyError::Upstream {
                status: status.as_u16(),
            });
        }
        let parsed: SubscriptionResponse = response.json().await?;
        let attrs = parsed.data.attributes;
        let sub_status = SubscriptionStatus::parse(&attrs.status).ok_or_else(|| {
            LemonSqueezyError::Parse(patom::types::ParseError::Malformed {
                field: "subscription.status",
                detail: "unknown Lemon Squeezy status",
            })
        })?;
        Ok(RemoteSubscription {
            variant_id: attrs
                .variant_id
                .map(|v| LsVariantId::try_from(v.to_string()))
                .transpose()?,
            customer_id: attrs
                .customer_id
                .map(|c| LsCustomerId::try_from(c.to_string()))
                .transpose()?,
            status: sub_status,
            current_period_end: attrs.renews_at.or(attrs.ends_at),
        })
    }
}

/// Minimal slice of the JSON:API subscription response we read.
#[derive(Deserialize)]
struct SubscriptionResponse {
    data: SubscriptionResponseData,
}
#[derive(Deserialize)]
struct SubscriptionResponseData {
    attributes: SubscriptionResponseAttrs,
}
#[derive(Deserialize)]
struct SubscriptionResponseAttrs {
    status: String,
    #[serde(default)]
    variant_id: Option<i64>,
    #[serde(default)]
    customer_id: Option<i64>,
    #[serde(default)]
    renews_at: Option<DateTime<Utc>>,
    #[serde(default)]
    ends_at: Option<DateTime<Utc>>,
}
