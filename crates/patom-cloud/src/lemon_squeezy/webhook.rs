//! `POST /webhooks/lemon-squeezy` — the inbound Lemon Squeezy webhook.
//!
//! Public route (no cookie gate): it authenticates itself by HMAC, exactly
//! like the Slack webhook. The handler verifies the `X-Signature`, parses the
//! event, dedupes by a body digest, and applies subscription state. Bounded by
//! `DefaultBodyLimit` + a handle timeout (CLAUDE.md §5).

use std::sync::Arc;

use axum::Router;
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Extension};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::post;
use patom::auth::OrgId;
use patom::http::AppState;
use patom::types::ParseError;
use sha2::{Digest, Sha256};
use tracing::{error, warn};
use uuid::Uuid;

use super::deps::CloudDeps;
use super::error::LemonSqueezyError;
use super::limits::{MAX_WEBHOOK_BODY_BYTES, WEBHOOK_HANDLE_TIMEOUT};
use super::payload::WebhookEnvelope;
use super::types::LsEventId;
use super::{lifecycle, verify};

/// Webhook route path. Public — merged into the unauthenticated group.
pub const WEBHOOK_PATH: &str = "/webhooks/lemon-squeezy";

/// The public webhook router, carrying its dependencies via an `Extension`
/// layer and a hard body-size cap.
pub fn webhook_router(deps: Arc<CloudDeps>) -> Router<AppState> {
    Router::new()
        .route(WEBHOOK_PATH, post(handle))
        .layer(Extension(deps))
        .layer(DefaultBodyLimit::max(MAX_WEBHOOK_BODY_BYTES))
}

#[tracing::instrument(skip_all, name = "lemon_squeezy.webhook")]
async fn handle(
    Extension(deps): Extension<Arc<CloudDeps>>,
    headers: HeaderMap,
    body: Bytes,
) -> StatusCode {
    let outcome =
        match tokio::time::timeout(WEBHOOK_HANDLE_TIMEOUT, process(&deps, &headers, &body)).await {
            Ok(result) => result,
            Err(_elapsed) => {
                error!(event = "lemon_squeezy.webhook.timeout");
                // 500 so Lemon Squeezy retries.
                return StatusCode::INTERNAL_SERVER_ERROR;
            }
        };
    match outcome {
        Ok(status) => status,
        // Bad signature / malformed body won't improve on retry → 4xx.
        Err(LemonSqueezyError::SignatureMismatch) => {
            warn!(event = "lemon_squeezy.webhook.bad_signature");
            StatusCode::UNAUTHORIZED
        }
        Err(LemonSqueezyError::Parse(e)) => {
            warn!(error = %e, event = "lemon_squeezy.webhook.bad_payload");
            StatusCode::BAD_REQUEST
        }
        // Storage failure is transient → 500 so the event is redelivered.
        Err(e) => {
            error!(error = ?e, event = "lemon_squeezy.webhook.failed");
            StatusCode::INTERNAL_SERVER_ERROR
        }
    }
}

async fn process(
    deps: &CloudDeps,
    headers: &HeaderMap,
    body: &Bytes,
) -> Result<StatusCode, LemonSqueezyError> {
    let signature = headers
        .get("X-Signature")
        .and_then(|v| v.to_str().ok())
        .ok_or(LemonSqueezyError::SignatureMismatch)?;
    verify::verify_signature(&deps.config.webhook_secret, signature, body)?;

    let envelope: WebhookEnvelope = serde_json::from_slice(body).map_err(|_| {
        LemonSqueezyError::Parse(ParseError::Malformed {
            field: "webhook_body",
            detail: "not valid Lemon Squeezy JSON",
        })
    })?;

    let event_id = body_event_id(body);
    let org = parse_org(&envelope)?;

    // Idempotency: a redelivery of the same (byte-identical) payload records no
    // new row and is acked without re-applying. The rare record-ok/apply-fail
    // gap is closed by the reconciliation poll.
    if !deps.subscriptions.record_event_once(&event_id, org).await? {
        return Ok(StatusCode::OK);
    }

    lifecycle::apply(deps, &envelope, org).await?;
    Ok(StatusCode::OK)
}

/// The idempotency key: the SHA-256 of the raw body. Lemon Squeezy webhooks
/// carry no stable event id, but a redelivery is byte-identical, so the digest
/// dedupes redeliveries while staying distinct across events.
fn body_event_id(body: &[u8]) -> LsEventId {
    let digest = Sha256::digest(body);
    let hex = verify::hex_encode(&digest);
    LsEventId::try_from(hex).expect("invariant: 64-char hex digest is a valid, bounded id")
}

/// Resolve `meta.custom_data.org_id` to an [`OrgId`]. `None` when absent;
/// `Err` when present but not a UUID (a malformed payload).
fn parse_org(envelope: &WebhookEnvelope) -> Result<Option<OrgId>, LemonSqueezyError> {
    let Some(raw) = envelope
        .meta
        .custom_data
        .as_ref()
        .and_then(|c| c.org_id.as_deref())
    else {
        return Ok(None);
    };
    let uuid = Uuid::parse_str(raw).map_err(|_| {
        LemonSqueezyError::Parse(ParseError::Malformed {
            field: "custom_data.org_id",
            detail: "not a UUID",
        })
    })?;
    Ok(Some(OrgId::from(uuid)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    use hmac::{Hmac, Mac};
    use patom::auth::OrgId;
    use patom::clock::SystemClock;
    use patom::types::SecretString;
    use sha2::Sha256;
    use sqlx::PgPool;
    use tower::ServiceExt;

    use crate::lemon_squeezy::client::{HttpLemonSqueezyClient, LEMON_SQUEEZY_API_BASE};
    use crate::lemon_squeezy::config::LemonSqueezyConfig;
    use crate::lemon_squeezy::pg_store::PgSubscriptionStore;
    use crate::lemon_squeezy::types::{LsVariantId, Plan};

    const SECRET: &str = "test_webhook_secret";
    const VARIANT: &str = "555";

    async fn deps(pool: &PgPool) -> Arc<CloudDeps> {
        crate::run_migrations(pool).await.expect("cloud migrations");
        let mut variants = HashMap::new();
        variants.insert(
            LsVariantId::try_from(VARIANT).expect("variant"),
            Plan::Starter,
        );
        let config = LemonSqueezyConfig::new(
            SecretString::try_from(SECRET.to_string()).expect("secret"),
            SecretString::try_from("api".to_string()).expect("api key"),
            "store_1".to_string(),
            variants,
        );
        Arc::new(CloudDeps {
            subscriptions: Arc::new(PgSubscriptionStore::new(
                pool.clone(),
                SystemClock::shared(),
            )),
            checkout_client: Arc::new(HttpLemonSqueezyClient::new(
                reqwest::Client::new(),
                SecretString::try_from("api".to_string()).expect("api key"),
                LEMON_SQUEEZY_API_BASE.to_string(),
            )),
            config,
            clock: SystemClock::shared(),
            app_base_url: None,
        })
    }

    fn app(deps: Arc<CloudDeps>) -> Router {
        Router::new()
            .route(WEBHOOK_PATH, post(handle))
            .layer(Extension(deps))
    }

    fn sign(body: &[u8]) -> String {
        let mut mac = Hmac::<Sha256>::new_from_slice(SECRET.as_bytes()).expect("key");
        mac.update(body);
        verify::hex_encode(&mac.finalize().into_bytes())
    }

    fn subscription_created(org: OrgId) -> Vec<u8> {
        format!(
            r#"{{"meta":{{"event_name":"subscription_created","custom_data":{{"org_id":"{org}"}}}},
                "data":{{"id":"sub_42","attributes":{{"customer_id":7,"variant_id":{VARIANT},
                "status":"active","renews_at":"2026-07-01T00:00:00.000000Z"}}}}}}"#
        )
        .into_bytes()
    }

    fn post_req(body: Vec<u8>, signature: &str) -> axum::http::Request<axum::body::Body> {
        axum::http::Request::builder()
            .method("POST")
            .uri(WEBHOOK_PATH)
            .header("X-Signature", signature)
            .header("content-type", "application/json")
            .body(axum::body::Body::from(body))
            .expect("request")
    }

    #[sqlx::test(migrations = "../patom-core/migrations")]
    async fn valid_subscription_created_persists(pool: PgPool) {
        let deps = deps(&pool).await;
        let org = OrgId::new();
        let body = subscription_created(org);
        let sig = sign(&body);

        let res = app(deps.clone())
            .oneshot(post_req(body, &sig))
            .await
            .expect("response");
        assert_eq!(res.status(), StatusCode::OK);

        let got = deps
            .subscriptions
            .read_for_org(org)
            .await
            .expect("read")
            .expect("a subscription");
        assert_eq!(got.plan, Plan::Starter);
        assert_eq!(got.ls_subscription_id.as_str(), "sub_42");
        assert!(got.current_period_end.is_some());
    }

    #[sqlx::test(migrations = "../patom-core/migrations")]
    async fn redelivery_is_deduped(pool: PgPool) {
        let deps = deps(&pool).await;
        let org = OrgId::new();
        let body = subscription_created(org);
        let sig = sign(&body);

        for _ in 0..2 {
            let res = app(deps.clone())
                .oneshot(post_req(body.clone(), &sig))
                .await
                .expect("response");
            assert_eq!(res.status(), StatusCode::OK);
        }

        let count: i64 =
            sqlx::query_scalar("SELECT count(*) FROM cloud.subscriptions WHERE org_id = $1")
                .bind(org)
                .fetch_one(&pool)
                .await
                .expect("count");
        assert_eq!(
            count, 1,
            "a redelivered webhook must not create a second row"
        );
    }

    #[sqlx::test(migrations = "../patom-core/migrations")]
    async fn bad_signature_is_rejected(pool: PgPool) {
        let deps = deps(&pool).await;
        let body = subscription_created(OrgId::new());

        let res = app(deps)
            .oneshot(post_req(body, "deadbeef"))
            .await
            .expect("response");
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    }
}
