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

// The agent-count gate is no longer a free pre-flight helper: enforcement
// moved into `AgentStore` (issue #131), where the count-then-insert runs in one
// transaction under a per-org advisory lock, so every creation path is covered
// and the old count-then-insert TOCTOU is closed. `agent_limit` above is still
// the seam that resolves the cap; the store consults it directly.

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
        AgentLimit, Entitlements, Feature, LicenseError, UnlimitedEntitlements, require_feature,
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
    async fn capped_policy_reports_its_ceiling() {
        // The agent-count *enforcement* now lives in `AgentStore` (tested end
        // to end against Postgres in `tests/entitlements_gate.rs` and
        // `tests/pg_agent_store.rs`); here we only assert the seam reports the
        // ceiling a capped policy resolves to.
        let ent = CappedEntitlements { max: 3 };
        assert_eq!(ent.agent_limit(OrgId::new()).await, AgentLimit::Max(3));
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
