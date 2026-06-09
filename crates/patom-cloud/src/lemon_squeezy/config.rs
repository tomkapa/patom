//! Process-wide Lemon Squeezy configuration.
//!
//! Secrets are [`SecretString`] so a stray `Debug` cannot leak them. The
//! variant→plan map is config-driven, since Lemon Squeezy variant ids differ
//! between test and production stores. Loaded through the same serde `config`
//! boundary patom-core uses ([`LemonSqueezyConfig::from_env`] →
//! `Environment` source → [`RawLemonSqueezy`]) — never `std::env::var`
//! directly — keeping all billing config inside this crate.

use std::collections::HashMap;

use config::{Config, Environment};
use patom::AppError;
use patom::types::SecretString;
use serde::Deserialize;

use super::types::{LsStoreId, LsVariantId, Plan};

/// Raw `PATOM_LEMON_SQUEEZY_*` env shape, deserialized once at the boundary by
/// the `config` crate (same pattern as core's `RawSettings`). Validated into
/// [`LemonSqueezyConfig`] via [`LemonSqueezyConfig::from_raw`].
//
// The shared `patom_lemon_squeezy_` prefix is required: each field name must
// match its env var for the `config` crate's `Environment::default()` source,
// exactly as core's `RawSettings` does it. Renaming to drop the prefix would
// break deserialization.
#[allow(clippy::struct_field_names)]
#[derive(Debug, Default, Deserialize)]
struct RawLemonSqueezy {
    #[serde(default)]
    patom_lemon_squeezy_api_key: Option<SecretString>,
    #[serde(default)]
    patom_lemon_squeezy_webhook_secret: Option<SecretString>,
    #[serde(default)]
    patom_lemon_squeezy_store_id: Option<String>,
    #[serde(default)]
    patom_lemon_squeezy_variant_starter: Option<String>,
    #[serde(default)]
    patom_lemon_squeezy_variant_growth: Option<String>,
    #[serde(default)]
    patom_lemon_squeezy_variant_scale: Option<String>,
    #[serde(default)]
    patom_lemon_squeezy_variant_enterprise: Option<String>,
}

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

    /// Load from the environment via the `config` crate. Returns `None` when
    /// Lemon Squeezy is unconfigured (the credential trio absent → no billing),
    /// `Err` on a partial set or a malformed value.
    ///
    /// Reads `PATOM_LEMON_SQUEEZY_API_KEY`, `_WEBHOOK_SECRET`, `_STORE_ID`
    /// (all-or-nothing) plus the optional per-plan variant ids
    /// `_VARIANT_STARTER` / `_GROWTH` / `_SCALE` / `_ENTERPRISE`.
    ///
    /// # Errors
    /// [`AppError::Misconfigured`] if the env can't be deserialized, the set is
    /// partial, or a value fails its newtype constructor.
    pub fn from_env() -> Result<Option<Self>, AppError> {
        let raw: RawLemonSqueezy = Config::builder()
            .add_source(Environment::default())
            .build()
            .and_then(config::Config::try_deserialize)
            .map_err(|e| AppError::Misconfigured(format!("lemon squeezy config: {e}")))?;
        Self::from_raw(raw)
    }

    /// Validate the raw env shape into a config (the all-or-nothing rule + the
    /// variant→plan map). Split from [`Self::from_env`] so it is unit-testable
    /// without touching the process environment.
    fn from_raw(raw: RawLemonSqueezy) -> Result<Option<Self>, AppError> {
        let misconf = |e: patom::types::ParseError| AppError::Misconfigured(e.to_string());
        // Empty/whitespace env values are treated as unset.
        let norm = |v: Option<String>| v.map(|s| s.trim().to_owned()).filter(|s| !s.is_empty());

        match (
            raw.patom_lemon_squeezy_api_key,
            raw.patom_lemon_squeezy_webhook_secret,
            norm(raw.patom_lemon_squeezy_store_id),
        ) {
            (None, None, None) => Ok(None),
            (Some(api_key), Some(webhook_secret), Some(store_id)) => {
                let store_id = LsStoreId::try_from(store_id).map_err(misconf)?;
                let mut variant_plans = HashMap::new();
                for (raw_variant, plan) in [
                    (norm(raw.patom_lemon_squeezy_variant_starter), Plan::Starter),
                    (norm(raw.patom_lemon_squeezy_variant_growth), Plan::Growth),
                    (norm(raw.patom_lemon_squeezy_variant_scale), Plan::Scale),
                    (
                        norm(raw.patom_lemon_squeezy_variant_enterprise),
                        Plan::Enterprise,
                    ),
                ] {
                    let Some(raw_variant) = raw_variant else {
                        continue;
                    };
                    let variant = LsVariantId::try_from(raw_variant.clone()).map_err(misconf)?;
                    // Fail fast rather than let the last writer win: a variant id
                    // mapped to two plans would misroute billing entitlements.
                    if let Some(existing) = variant_plans.insert(variant, plan) {
                        return Err(AppError::Misconfigured(format!(
                            "lemon squeezy: variant id {raw_variant:?} is configured for multiple \
                             plans ({existing:?} and {plan:?})"
                        )));
                    }
                }
                Ok(Some(Self::new(
                    webhook_secret,
                    api_key,
                    store_id,
                    variant_plans,
                )))
            }
            _ => Err(AppError::Misconfigured(
                "lemon squeezy: set all of PATOM_LEMON_SQUEEZY_API_KEY, \
                 PATOM_LEMON_SQUEEZY_WEBHOOK_SECRET, PATOM_LEMON_SQUEEZY_STORE_ID — or none"
                    .to_string(),
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(
        api_key: Option<&str>,
        webhook_secret: Option<&str>,
        store_id: Option<&str>,
        starter: Option<&str>,
        growth: Option<&str>,
    ) -> RawLemonSqueezy {
        let secret = |s: &str| SecretString::try_from(s.to_string()).expect("secret");
        RawLemonSqueezy {
            patom_lemon_squeezy_api_key: api_key.map(secret),
            patom_lemon_squeezy_webhook_secret: webhook_secret.map(secret),
            patom_lemon_squeezy_store_id: store_id.map(str::to_string),
            patom_lemon_squeezy_variant_starter: starter.map(str::to_string),
            patom_lemon_squeezy_variant_growth: growth.map(str::to_string),
            patom_lemon_squeezy_variant_scale: None,
            patom_lemon_squeezy_variant_enterprise: None,
        }
    }

    fn variant(id: &str) -> LsVariantId {
        LsVariantId::try_from(id).expect("variant id")
    }

    #[test]
    fn unconfigured_is_none() {
        let cfg = LemonSqueezyConfig::from_raw(RawLemonSqueezy::default()).expect("ok");
        assert!(cfg.is_none());
    }

    #[test]
    fn partial_config_is_rejected() {
        // API key set, but webhook secret + store id missing → partial.
        let err = LemonSqueezyConfig::from_raw(raw(Some("k"), None, None, None, None))
            .expect_err("partial");
        assert!(matches!(err, AppError::Misconfigured(_)));
    }

    #[test]
    fn full_config_maps_variants_to_plans() {
        let cfg = LemonSqueezyConfig::from_raw(raw(
            Some("k"),
            Some("s"),
            Some("store_1"),
            Some("111"),
            // Whitespace is normalised to unset.
            Some("  "),
        ))
        .expect("config")
        .expect("configured");
        assert_eq!(cfg.store_id.as_str(), "store_1");
        assert_eq!(cfg.plan_for(&variant("111")), Some(Plan::Starter));
        assert_eq!(cfg.plan_for(&variant("999")), None);
    }

    #[test]
    fn empty_store_id_is_treated_as_unset() {
        // store_id is whitespace, the other two set → the trio is no longer
        // complete, so it is a partial config.
        let err = LemonSqueezyConfig::from_raw(raw(Some("k"), Some("s"), Some("  "), None, None))
            .expect_err("partial");
        assert!(matches!(err, AppError::Misconfigured(_)));
    }

    #[test]
    fn variant_shared_by_two_plans_is_rejected() {
        let err = LemonSqueezyConfig::from_raw(raw(
            Some("k"),
            Some("s"),
            Some("store_1"),
            Some("dup"),
            Some("dup"),
        ))
        .expect_err("duplicate variant");
        assert!(matches!(err, AppError::Misconfigured(_)));
    }
}
