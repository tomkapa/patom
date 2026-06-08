//! Process-wide Lemon Squeezy configuration.
//!
//! Secrets are [`SecretString`] so a stray `Debug` cannot leak them. The
//! variant→plan map is config-driven, since Lemon Squeezy variant ids differ
//! between test and production stores. Built from core's
//! [`patom::LemonSqueezySettings`] (loaded through the one serde config
//! boundary in `patom-core`) via [`LemonSqueezyConfig::from_settings`] — the
//! cloud crate never reads the environment directly.

use std::collections::HashMap;

use patom::AppError;
use patom::LemonSqueezySettings;
use patom::types::SecretString;

use super::types::{LsStoreId, LsVariantId, Plan};

/// Everything the Lemon Squeezy subsystem needs from configuration.
#[derive(Debug, Clone)]
pub struct LemonSqueezyConfig {
    /// Webhook signing secret — verifies the `X-Signature` HMAC.
    pub webhook_secret: SecretString,
    /// API key for the Lemon Squeezy REST API (checkout creation,
    /// reconciliation).
    pub api_key: SecretString,
    /// Store id the checkout is created against.
    pub store_id: LsStoreId,
    /// Maps a Lemon Squeezy variant id to the [`Plan`] it sells.
    variant_plans: HashMap<LsVariantId, Plan>,
}

impl LemonSqueezyConfig {
    #[must_use]
    pub fn new(
        webhook_secret: SecretString,
        api_key: SecretString,
        store_id: LsStoreId,
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

    /// Build from core's parsed [`patom::LemonSqueezySettings`] (the all-or-
    /// nothing config already validated at the serde boundary). Maps the
    /// per-plan variant ids into the lookup table.
    ///
    /// # Errors
    /// [`AppError::Misconfigured`] if a store / variant id fails its newtype
    /// constructor, or if two plans are configured with the same variant id
    /// (which would silently misroute entitlements).
    pub fn from_settings(settings: &LemonSqueezySettings) -> Result<Self, AppError> {
        let misconf = |e: patom::types::ParseError| AppError::Misconfigured(e.to_string());
        let store_id = LsStoreId::try_from(settings.store_id.clone()).map_err(misconf)?;

        let mut variant_plans = HashMap::new();
        for (raw, plan) in [
            (&settings.variant_starter, Plan::Starter),
            (&settings.variant_growth, Plan::Growth),
            (&settings.variant_scale, Plan::Scale),
            (&settings.variant_enterprise, Plan::Enterprise),
        ] {
            let Some(raw) = raw else { continue };
            let variant = LsVariantId::try_from(raw.clone()).map_err(misconf)?;
            // Fail fast rather than let the last writer win: a variant id mapped
            // to two plans would misroute billing entitlements.
            if let Some(existing) = variant_plans.insert(variant, plan) {
                return Err(AppError::Misconfigured(format!(
                    "lemon squeezy: variant id {raw:?} is configured for multiple plans \
                     ({existing:?} and {plan:?})"
                )));
            }
        }
        Ok(Self::new(
            settings.webhook_secret.clone(),
            settings.api_key.clone(),
            store_id,
            variant_plans,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings(starter: Option<&str>, growth: Option<&str>) -> LemonSqueezySettings {
        LemonSqueezySettings {
            api_key: SecretString::try_from("k".to_string()).expect("key"),
            webhook_secret: SecretString::try_from("s".to_string()).expect("secret"),
            store_id: "store_1".to_string(),
            variant_starter: starter.map(str::to_string),
            variant_growth: growth.map(str::to_string),
            variant_scale: None,
            variant_enterprise: None,
        }
    }

    fn variant(id: &str) -> LsVariantId {
        LsVariantId::try_from(id).expect("variant id")
    }

    #[test]
    fn from_settings_maps_variants_to_their_plans() {
        let cfg =
            LemonSqueezyConfig::from_settings(&settings(Some("111"), Some("222"))).expect("config");
        assert_eq!(cfg.plan_for(&variant("111")), Some(Plan::Starter));
        assert_eq!(cfg.plan_for(&variant("222")), Some(Plan::Growth));
        assert_eq!(cfg.plan_for(&variant("999")), None);
        assert_eq!(cfg.store_id.as_str(), "store_1");
    }

    #[test]
    fn from_settings_rejects_a_variant_shared_by_two_plans() {
        // The same variant id under two plans must fail, not silently overwrite
        // (which would misroute entitlements).
        let err = LemonSqueezyConfig::from_settings(&settings(Some("dup"), Some("dup")))
            .expect_err("duplicate variant");
        assert!(matches!(err, AppError::Misconfigured(_)));
    }
}
