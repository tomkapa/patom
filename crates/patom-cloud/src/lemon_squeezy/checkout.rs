//! `POST /api/billing/checkout` — start an upgrade.
//!
//! Private route: it inherits `require_principal` / CSRF from the core
//! authenticated group, so the caller is an authenticated user and we attribute
//! the checkout to their active org. The handler is a thin wrapper over
//! [`create_checkout_for`], which holds the logic and is unit-tested with a fake
//! client (no HTTP, no DB).

use std::sync::Arc;

use axum::extract::Extension;
use axum::http::StatusCode;
use axum::routing::post;
use axum::{Json, Router};
use patom::auth::{OrgId, Principal};
use patom::http::AppState;
use serde::{Deserialize, Serialize};

use super::client::{CheckoutCreate, LsCheckoutClient};
use super::config::LemonSqueezyConfig;
use super::deps::CloudDeps;
use super::error::LemonSqueezyError;
use super::types::LsVariantId;

/// Checkout route path (mounted under the `/api` private group → final
/// `/api/billing/checkout`).
pub const CHECKOUT_PATH: &str = "/billing/checkout";

/// Request body: the variant (price) the user picked. The FE knows the catalog
/// of variants (monthly vs annual, per plan); the server only sells variants it
/// recognises.
#[derive(Debug, Deserialize)]
pub struct CheckoutRequest {
    pub variant_id: String,
}

/// Response: the hosted checkout URL to redirect the browser to.
#[derive(Debug, Serialize)]
pub struct CheckoutResponse {
    pub url: String,
}

/// The private checkout router, carrying its deps via an `Extension` layer.
pub fn checkout_router(deps: Arc<CloudDeps>) -> Router<AppState> {
    Router::new()
        .route(CHECKOUT_PATH, post(handle))
        .layer(Extension(deps))
}

#[tracing::instrument(skip_all, name = "lemon_squeezy.checkout", fields(patom.org.id = %principal.active_org_id))]
async fn handle(
    principal: Principal,
    Extension(deps): Extension<Arc<CloudDeps>>,
    Json(req): Json<CheckoutRequest>,
) -> Result<Json<CheckoutResponse>, StatusCode> {
    match create_checkout_for(
        &deps.config,
        deps.checkout_client.as_ref(),
        deps.app_base_url.as_deref(),
        principal.active_org_id,
        &req,
    )
    .await
    {
        Ok(resp) => Ok(Json(resp)),
        Err(e) => {
            // Emit the error event inside the span before mapping to a status —
            // the OTel bridge sets span status from it (CLAUDE.md §2).
            tracing::error!(error = ?e, event = "lemon_squeezy.checkout.failed");
            Err(status_for(e))
        }
    }
}

/// Validate the variant, build the checkout, and return its URL.
///
/// The org comes from the authenticated principal; `custom_data.org_id` carries
/// it through to the webhook so the resulting subscription is attributed
/// correctly.
///
/// # Errors
/// [`LemonSqueezyError::UnknownVariant`] if the variant isn't sold here;
/// otherwise whatever the client returns.
pub async fn create_checkout_for(
    config: &LemonSqueezyConfig,
    client: &dyn LsCheckoutClient,
    app_base_url: Option<&str>,
    org: OrgId,
    req: &CheckoutRequest,
) -> Result<CheckoutResponse, LemonSqueezyError> {
    let variant = LsVariantId::try_from(req.variant_id.clone())?;
    // Only sell variants we recognise — otherwise we'd create a checkout whose
    // webhook we can't map to a plan.
    if config.plan_for(&variant).is_none() {
        return Err(LemonSqueezyError::UnknownVariant);
    }
    let redirect_url = app_base_url.map(|base| format!("{base}/billing/success"));
    let url = client
        .create_checkout(CheckoutCreate {
            store_id: config.store_id.clone(),
            variant,
            org_id: org,
            redirect_url,
        })
        .await?;
    Ok(CheckoutResponse { url })
}

/// HTTP status for a checkout failure.
fn status_for(err: LemonSqueezyError) -> StatusCode {
    match err {
        LemonSqueezyError::UnknownVariant | LemonSqueezyError::Parse(_) => StatusCode::BAD_REQUEST,
        LemonSqueezyError::Http(_) | LemonSqueezyError::Upstream { .. } => StatusCode::BAD_GATEWAY,
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex;

    use async_trait::async_trait;
    use patom::types::SecretString;

    use super::super::client::CheckoutCreate;
    use super::super::types::{LsStoreId, Plan};

    const VARIANT: &str = "555";

    /// Captures the request and returns a canned URL — no HTTP.
    #[derive(Debug, Default)]
    struct FakeClient {
        seen: Mutex<Option<CheckoutCreate>>,
    }

    #[async_trait]
    impl LsCheckoutClient for FakeClient {
        async fn create_checkout(&self, req: CheckoutCreate) -> Result<String, LemonSqueezyError> {
            *self.seen.lock().expect("lock") = Some(req);
            Ok("https://checkout.example/abc".to_string())
        }
        async fn get_subscription(
            &self,
            _id: &super::super::types::LsSubscriptionId,
        ) -> Result<super::super::client::RemoteSubscription, LemonSqueezyError> {
            // Not exercised by the checkout tests.
            Err(LemonSqueezyError::Upstream {
                status: reqwest::StatusCode::NOT_IMPLEMENTED,
            })
        }
    }

    fn config() -> LemonSqueezyConfig {
        let mut variants = HashMap::new();
        variants.insert(
            LsVariantId::try_from(VARIANT).expect("variant"),
            Plan::Scale,
        );
        LemonSqueezyConfig::new(
            SecretString::try_from("secret".to_string()).expect("secret"),
            SecretString::try_from("api".to_string()).expect("api"),
            LsStoreId::try_from("store_9").expect("store id"),
            variants,
        )
    }

    #[tokio::test]
    async fn known_variant_creates_checkout_with_org_custom_data() {
        let cfg = config();
        let client = FakeClient::default();
        let org = OrgId::new();
        let req = CheckoutRequest {
            variant_id: VARIANT.to_string(),
        };

        let res = create_checkout_for(&cfg, &client, Some("https://app.example"), org, &req)
            .await
            .expect("checkout");
        assert_eq!(res.url, "https://checkout.example/abc");

        let seen = client.seen.lock().expect("lock").clone().expect("captured");
        assert_eq!(seen.org_id, org, "org must be attached as custom data");
        assert_eq!(seen.store_id.as_str(), "store_9");
        assert_eq!(seen.variant.as_str(), VARIANT);
        assert_eq!(
            seen.redirect_url.as_deref(),
            Some("https://app.example/billing/success"),
        );
    }

    #[tokio::test]
    async fn unknown_variant_is_refused() {
        let cfg = config();
        let client = FakeClient::default();
        let req = CheckoutRequest {
            variant_id: "999".to_string(),
        };

        let err = create_checkout_for(&cfg, &client, None, OrgId::new(), &req)
            .await
            .expect_err("unknown variant");
        assert!(matches!(err, LemonSqueezyError::UnknownVariant));
        assert!(
            client.seen.lock().expect("lock").is_none(),
            "an unknown variant must never reach the client",
        );
    }
}
