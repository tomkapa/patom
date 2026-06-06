//! Shared dependencies for the Lemon Squeezy route handlers.
//!
//! Carried into the webhook / checkout routers via an `axum::Extension` layer
//! rather than core's `AppState`, so no billing type leaks into the core HTTP
//! state (the open-core boundary).

use patom::clock::SharedClock;

use super::config::LemonSqueezyConfig;
use super::store::SharedSubscriptionStore;

/// Everything the cloud route handlers need, behind one `Arc`.
#[derive(Debug)]
pub struct CloudDeps {
    pub subscriptions: SharedSubscriptionStore,
    pub config: LemonSqueezyConfig,
    pub clock: SharedClock,
}
