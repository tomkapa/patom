//! Shared dependencies for the Lemon Squeezy route handlers.
//!
//! Carried into the webhook / checkout routers via an `axum::Extension` layer
//! rather than core's `AppState`, so no billing type leaks into the core HTTP
//! state (the open-core boundary).

use patom::clock::SharedClock;

use super::client::SharedCheckoutClient;
use super::config::LemonSqueezyConfig;
use super::store::SharedSubscriptionStore;

/// Everything the cloud route handlers need, behind one `Arc`.
#[derive(Debug)]
pub struct CloudDeps {
    pub subscriptions: SharedSubscriptionStore,
    pub checkout_client: SharedCheckoutClient,
    pub config: LemonSqueezyConfig,
    pub clock: SharedClock,
    /// Base URL the buyer is redirected back to after checkout (derived from
    /// the app's web base URL). `None` falls back to the store's default.
    pub app_base_url: Option<String>,
}
