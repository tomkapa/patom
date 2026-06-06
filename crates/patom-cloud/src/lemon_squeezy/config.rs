//! Process-wide Lemon Squeezy configuration.
//!
//! Secrets are [`SecretString`] so a stray `Debug` cannot leak them. The
//! variant→plan map is config-driven (env), since Lemon Squeezy variant ids
//! differ between test and production stores. [`LemonSqueezyConfig::from_env`]
//! reads it all from `PATOM_LEMON_SQUEEZY_*`, keeping every billing config name
//! in `patom-cloud` (the open-core boundary) — core's `Settings` stays
//! billing-free.

use std::collections::HashMap;

use patom::AppError;
use patom::types::SecretString;

use super::types::{LsVariantId, Plan};

/// Read an env var, treating empty/whitespace as unset.
fn env_opt(key: &str) -> Option<String> {
    match std::env::var(key) {
        Ok(v) if !v.trim().is_empty() => Some(v),
        _ => None,
    }
}

/// Everything the Lemon Squeezy subsystem needs from configuration.
#[derive(Debug, Clone)]
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

    /// Load from the environment. Returns `None` when Lemon Squeezy is
    /// unconfigured (the core trio absent → no billing), `Err` on a partial
    /// set. Keeps all billing config in `patom-cloud` (the open-core boundary)
    /// — secrets come from `patom-infra`, never code.
    ///
    /// Reads `PATOM_LEMON_SQUEEZY_API_KEY`, `_WEBHOOK_SECRET`, `_STORE_ID`
    /// (all-or-nothing) plus the optional per-plan variant ids
    /// `_VARIANT_STARTER` / `_GROWTH` / `_SCALE` / `_ENTERPRISE`.
    ///
    /// # Errors
    /// [`AppError::Misconfigured`] if only some of the required trio is set, or
    /// a secret / variant id fails its constructor.
    pub fn from_env() -> Result<Option<Self>, AppError> {
        let misconf = |e: patom::types::ParseError| AppError::Misconfigured(e.to_string());
        match (
            env_opt("PATOM_LEMON_SQUEEZY_API_KEY"),
            env_opt("PATOM_LEMON_SQUEEZY_WEBHOOK_SECRET"),
            env_opt("PATOM_LEMON_SQUEEZY_STORE_ID"),
        ) {
            (None, None, None) => Ok(None),
            (Some(api_key), Some(webhook_secret), Some(store_id)) => {
                let api_key = SecretString::try_from(api_key).map_err(misconf)?;
                let webhook_secret = SecretString::try_from(webhook_secret).map_err(misconf)?;
                let mut variant_plans = HashMap::new();
                for (key, plan) in [
                    ("PATOM_LEMON_SQUEEZY_VARIANT_STARTER", Plan::Starter),
                    ("PATOM_LEMON_SQUEEZY_VARIANT_GROWTH", Plan::Growth),
                    ("PATOM_LEMON_SQUEEZY_VARIANT_SCALE", Plan::Scale),
                    ("PATOM_LEMON_SQUEEZY_VARIANT_ENTERPRISE", Plan::Enterprise),
                ] {
                    if let Some(raw) = env_opt(key) {
                        variant_plans.insert(LsVariantId::try_from(raw).map_err(misconf)?, plan);
                    }
                }
                Ok(Some(Self::new(
                    api_key,
                    webhook_secret,
                    store_id,
                    variant_plans,
                )))
            }
            _ => Err(AppError::Misconfigured(
                "lemon squeezy: set all of PATOM_LEMON_SQUEEZY_API_KEY, \
                 PATOM_LEMON_SQUEEZY_WEBHOOK_SECRET, PATOM_LEMON_SQUEEZY_STORE_ID, or none"
                    .to_string(),
            )),
        }
    }
}
