//! Process-wide Lemon Squeezy configuration.
//!
//! Secrets are [`SecretString`] so a stray `Debug` cannot leak them. The
//! variant→plan map is config-driven (env), since Lemon Squeezy variant ids
//! differ between test and production stores. The env wiring that populates
//! this lives in patom-core's `Settings` (#131); this struct is what the cloud
//! crate consumes.

use std::collections::HashMap;

use patom::types::SecretString;

use super::types::{LsVariantId, Plan};

/// Everything the Lemon Squeezy subsystem needs from configuration.
#[derive(Debug)]
pub struct LemonSqueezyConfig {
    /// Webhook signing secret — verifies the `X-Signature` HMAC.
    pub webhook_secret: SecretString,
    /// API key for the Lemon Squeezy REST API (checkout creation,
    /// reconciliation). Used by the client (#131).
    pub api_key: SecretString,
    /// Store id the checkout is created against (#131).
    pub store_id: String,
    /// Maps a Lemon Squeezy variant id to the [`Plan`] it sells.
    variant_plans: HashMap<LsVariantId, Plan>,
}

impl LemonSqueezyConfig {
    #[must_use]
    pub fn new(
        webhook_secret: SecretString,
        api_key: SecretString,
        store_id: String,
        variant_plans: HashMap<LsVariantId, Plan>,
    ) -> Self {
        Self {
            webhook_secret,
            api_key,
            store_id,
            variant_plans,
        }
    }

    /// The plan a variant id sells, if configured. `None` for an unknown
    /// variant — the webhook acks-and-skips rather than guessing a plan.
    #[must_use]
    pub fn plan_for(&self, variant: &LsVariantId) -> Option<Plan> {
        self.variant_plans.get(variant).copied()
    }
}
