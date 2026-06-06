//! Entitlement seam — the single, typed place to ask "is this org allowed to
//! do X?".
//!
//! Core defines the [`Entitlements`] trait (the seam) and the permissive
//! [`UnlimitedEntitlements`] default. `patom-cloud` provides the concrete,
//! billing-backed impl that resolves an org to a paid tier and its agent cap
//! (#131); it is compiled only under `patom-server`'s `cloud` feature, so the
//! OSS / self-host binary links none of it. Core never imports billing.
//!
//! Two questions live behind one object-safe trait:
//! - [`Entitlements::agent_limit`] — the agent-count quota that monetization
//!   actually scales on. Wired into `POST /agents` via
//!   [`require_agent_capacity`].
//! - [`Entitlements::allows`] — a boolean [`Feature`] gate for capabilities a
//!   plan may withhold. Inert today (no real features gated); the hook exists
//!   so the first gate is an additive change.

mod error;
mod types;

pub use error::LicenseError;
pub use types::{AgentLimit, Feature};

use std::sync::Arc;

use async_trait::async_trait;

use crate::auth::OrgId;

/// Cheaply-cloneable, object-safe handle to the entitlement policy.
///
/// Stored on [`crate::http::AppState`]; the concrete impl behind it is the one
/// line a build swaps to change policy (OSS default vs. cloud vs. a future
/// conversion-driving self-host limit).
pub type SharedEntitlements = Arc<dyn Entitlements>;

/// The entitlement policy seam. Object-safe by construction (`&self`, no
/// generics, no `Self`-returning methods) so it can live behind a `dyn`.
///
/// Methods are `async` because the billing-backed `patom-cloud` impl (#131)
/// resolves an org's tier from Postgres on each call; the OSS default answers
/// from a constant and simply ignores the asynchrony.
#[async_trait]
pub trait Entitlements: std::fmt::Debug + Send + Sync + 'static {
    /// How many agents `org` may run. Drives the `POST /agents` gate.
    async fn agent_limit(&self, org: OrgId) -> AgentLimit;

    /// Whether `org`'s plan licenses `feature`. `true` for everything under
    /// the OSS default.
    async fn allows(&self, org: OrgId, feature: Feature) -> bool;
}

/// The permissive default: unlimited agents, every feature on.
///
/// This is the *current* OSS / self-host policy, not a fixed contract — a
/// future build may swap in a capped impl (here or in cloud) to limit
/// self-host and drive cloud conversion. Named for its behavior (`Unlimited`)
/// rather than its audience, so it stays honest if self-host later gets a
/// different, stricter impl.
#[derive(Debug, Default)]
pub struct UnlimitedEntitlements;

#[async_trait]
impl Entitlements for UnlimitedEntitlements {
    async fn agent_limit(&self, _org: OrgId) -> AgentLimit {
        AgentLimit::Unlimited
    }

    async fn allows(&self, _org: OrgId, _feature: Feature) -> bool {
        true
    }
}

/// Gate: refuse a new agent when `org` is at its [`AgentLimit`].
///
/// `current` is the org's present agent count. Returns
/// [`LicenseError::AgentLimitReached`] (→ 402) when the limit does not admit
/// one more. Inert under [`UnlimitedEntitlements`] (always admits).
///
/// # Errors
/// [`LicenseError::AgentLimitReached`] if `org` already holds its cap.
pub async fn require_agent_capacity(
    ent: &dyn Entitlements,
    org: OrgId,
    current: u32,
) -> Result<(), LicenseError> {
    match ent.agent_limit(org).await {
        AgentLimit::Unlimited => Ok(()),
        AgentLimit::Max(cap) if current < cap => Ok(()),
        // Only a `Max` ceiling can deny, so the cap surfaced on the 402 is
        // exactly the one that was hit.
        AgentLimit::Max(cap) => Err(LicenseError::AgentLimitReached { limit: cap }),
    }
}

/// Gate: refuse a [`Feature`] the org's plan does not license.
///
/// Inert today — [`UnlimitedEntitlements`] licenses everything — but exercised
/// by tests through a restrictive impl, and ready for the first real gate.
///
/// # Errors
/// [`LicenseError::FeatureNotLicensed`] if `ent` does not allow `feature`.
pub async fn require_feature(
    ent: &dyn Entitlements,
    org: OrgId,
    feature: Feature,
) -> Result<(), LicenseError> {
    if ent.allows(org, feature).await {
        return Ok(());
    }
    Err(LicenseError::FeatureNotLicensed { feature })
}

#[cfg(test)]
mod tests {
    use super::{
        AgentLimit, Entitlements, Feature, LicenseError, UnlimitedEntitlements,
        require_agent_capacity, require_feature,
    };
    use crate::auth::OrgId;

    /// A restrictive policy for exercising the deny paths the OSS default
    /// never takes.
    #[derive(Debug)]
    struct CappedEntitlements {
        max: u32,
    }

    #[async_trait::async_trait]
    impl Entitlements for CappedEntitlements {
        async fn agent_limit(&self, _org: OrgId) -> AgentLimit {
            AgentLimit::Max(self.max)
        }
        async fn allows(&self, _org: OrgId, _feature: Feature) -> bool {
            false
        }
    }

    #[tokio::test]
    async fn default_grants_unlimited_agents() {
        let ent = UnlimitedEntitlements;
        assert_eq!(ent.agent_limit(OrgId::new()).await, AgentLimit::Unlimited);
    }

    #[tokio::test]
    async fn default_allows_every_feature() {
        let ent = UnlimitedEntitlements;
        assert!(ent.allows(OrgId::new(), Feature::Reserved).await);
    }

    #[tokio::test]
    async fn default_capacity_gate_never_trips() {
        let ent = UnlimitedEntitlements;
        assert!(
            require_agent_capacity(&ent, OrgId::new(), 10_000)
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn capped_capacity_gate_denies_at_ceiling() {
        let ent = CappedEntitlements { max: 1 };
        let result = require_agent_capacity(&ent, OrgId::new(), 1).await;
        assert!(matches!(
            result,
            Err(LicenseError::AgentLimitReached { limit: 1 })
        ));
    }

    #[tokio::test]
    async fn capped_capacity_gate_admits_below_ceiling() {
        let ent = CappedEntitlements { max: 3 };
        assert!(require_agent_capacity(&ent, OrgId::new(), 2).await.is_ok());
    }

    #[tokio::test]
    async fn feature_gate_denies_under_restrictive_policy() {
        let ent = CappedEntitlements { max: 0 };
        let result = require_feature(&ent, OrgId::new(), Feature::Reserved).await;
        assert!(matches!(
            result,
            Err(LicenseError::FeatureNotLicensed {
                feature: Feature::Reserved
            })
        ));
    }
}
