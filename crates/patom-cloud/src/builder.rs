//! `LemonSqueezyCloud` — the concrete [`CloudBuilder`] patom-server injects
//! under `--features cloud` (#131).
//!
//! Ties the Lemon Squeezy pieces together: runs the `cloud` migrations, and
//! from the live runtime handles ([`CloudCtx`]) builds the subscription store,
//! the REST client, the billing-backed entitlement policy, and the webhook +
//! checkout routers. Core asks (via the seam); this answers.

use std::sync::Arc;

use async_trait::async_trait;
use patom::{AppError, CloudBuilder, CloudCtx, CloudParts, SharedEntitlements};
use sqlx::PgPool;

use crate::lemon_squeezy::client::{HttpLemonSqueezyClient, LEMON_SQUEEZY_API_BASE};
use crate::lemon_squeezy::config::LemonSqueezyConfig;
use crate::lemon_squeezy::deps::CloudDeps;
use crate::lemon_squeezy::entitlements::BillingEntitlements;
use crate::lemon_squeezy::pg_store::PgSubscriptionStore;
use crate::lemon_squeezy::store::SharedSubscriptionStore;
use crate::lemon_squeezy::{checkout, webhook};
use crate::run_migrations;

/// Cloud composition for the Lemon Squeezy billing edition.
#[derive(Debug)]
pub struct LemonSqueezyCloud {
    config: LemonSqueezyConfig,
    /// App web base URL (for the post-checkout redirect). `None` uses the
    /// store's default redirect.
    app_base_url: Option<String>,
}

impl LemonSqueezyCloud {
    #[must_use]
    pub fn new(config: LemonSqueezyConfig, app_base_url: Option<String>) -> Self {
        Self {
            config,
            app_base_url,
        }
    }
}

#[async_trait]
impl CloudBuilder for LemonSqueezyCloud {
    async fn migrate(&self, pool: &PgPool) -> Result<(), AppError> {
        run_migrations(pool).await
    }

    fn build(&self, ctx: CloudCtx) -> CloudParts {
        let subscriptions: SharedSubscriptionStore = Arc::new(PgSubscriptionStore::new(
            ctx.pool.clone(),
            ctx.clock.clone(),
        ));
        let checkout_client = Arc::new(HttpLemonSqueezyClient::new(
            ctx.http.clone(),
            self.config.api_key.clone(),
            LEMON_SQUEEZY_API_BASE.to_string(),
        ));
        let entitlements: SharedEntitlements = Arc::new(BillingEntitlements::new(
            subscriptions.clone(),
            ctx.clock.clone(),
        ));
        let deps = Arc::new(CloudDeps {
            subscriptions,
            checkout_client,
            config: self.config.clone(),
            clock: ctx.clock,
            app_base_url: self.app_base_url.clone(),
        });
        CloudParts {
            entitlements,
            public_routes: webhook::webhook_router(deps.clone()),
            private_routes: checkout::checkout_router(deps),
        }
    }
}
