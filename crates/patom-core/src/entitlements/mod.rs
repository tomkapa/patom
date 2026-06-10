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

use crate::auth::OrgId;
use crate::billing::GrantAmount;

/// Cheaply-cloneable, object-safe handle to the entitlement policy.
///
/// Stored on [`crate::http::AppState`]; the concrete impl behind it is the one
/// line a build swaps to change policy (OSS default vs. cloud vs. a future
/// conversion-driving self-host limit).
pub type SharedEntitlements = Arc<dyn Entitlements>;

/// The entitlement policy seam. Object-safe by construction (`&self`, no
/// generics, no `Self`-returning methods) so it can live behind a `dyn`.
pub trait Entitlements: std::fmt::Debug + Send + Sync + 'static {
    /// How many agents `org` may run. Drives the `POST /agents` gate.
    fn agent_limit(&self, org: OrgId) -> AgentLimit;

    /// Whether `org`'s plan licenses `feature`. `true` for everything under
    /// the OSS default.
    fn allows(&self, org: OrgId, feature: Feature) -> bool;

    /// Whether the free-credit gate is enforced for `org` (#154). `false` under
    /// the OSS default — self-host runs unmetered, so the credit balance is
    /// ignored and a turn never blocks on it. A cloud build returns `true`, so
    /// a zero balance gates further platform inference.
    fn credit_gate_active(&self, org: OrgId) -> bool;

    /// The automatic credit grant to seed a new `org` with, if any (#154). The
    /// launch-period signup promo: `Some($2)` on cloud, `None` under the OSS
    /// default and once the promo ends. Keyed `signup:{org_id}` by the caller
    /// so it fires exactly once per org.
    fn signup_grant(&self, org: OrgId) -> Option<GrantAmount>;
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

impl Entitlements for UnlimitedEntitlements {
    fn agent_limit(&self, _org: OrgId) -> AgentLimit {
        AgentLimit::Unlimited
    }

    fn allows(&self, _org: OrgId, _feature: Feature) -> bool {
        true
    }

    fn credit_gate_active(&self, _org: OrgId) -> bool {
        false
    }

    fn signup_grant(&self, _org: OrgId) -> Option<GrantAmount> {
        None
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
pub fn require_agent_capacity(
    ent: &dyn Entitlements,
    org: OrgId,
    current: u32,
) -> Result<(), LicenseError> {
    match ent.agent_limit(org) {
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
pub fn require_feature(
    ent: &dyn Entitlements,
    org: OrgId,
    feature: Feature,
) -> Result<(), LicenseError> {
    if ent.allows(org, feature) {
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
    use crate::billing::GrantAmount;

    /// A restrictive policy for exercising the deny paths the OSS default
    /// never takes. Models a paid/cloud-shaped tier: the credit gate is active
    /// and a signup grant fires.
    #[derive(Debug)]
    struct CappedEntitlements {
        max: u32,
    }

    impl Entitlements for CappedEntitlements {
        fn agent_limit(&self, _org: OrgId) -> AgentLimit {
            AgentLimit::Max(self.max)
        }
        fn allows(&self, _org: OrgId, _feature: Feature) -> bool {
            false
        }
        fn credit_gate_active(&self, _org: OrgId) -> bool {
            true
        }
        fn signup_grant(&self, _org: OrgId) -> Option<GrantAmount> {
            Some(GrantAmount::try_from(2_000_000).expect("positive"))
        }
    }

    #[test]
    fn default_grants_unlimited_agents() {
        let ent = UnlimitedEntitlements;
        assert_eq!(ent.agent_limit(OrgId::new()), AgentLimit::Unlimited);
    }

    #[test]
    fn default_allows_every_feature() {
        let ent = UnlimitedEntitlements;
        assert!(ent.allows(OrgId::new(), Feature::Reserved));
    }

    #[test]
    fn default_credit_gate_is_inactive_with_no_signup_grant() {
        // OSS / self-host: credits are ignored and no grant fires.
        let ent = UnlimitedEntitlements;
        assert!(!ent.credit_gate_active(OrgId::new()));
        assert!(ent.signup_grant(OrgId::new()).is_none());
    }

    #[test]
    fn paid_policy_activates_gate_and_grants() {
        let ent = CappedEntitlements { max: 1 };
        assert!(ent.credit_gate_active(OrgId::new()));
        assert_eq!(
            ent.signup_grant(OrgId::new()).map(GrantAmount::get),
            Some(2_000_000)
        );
    }

    #[test]
    fn default_capacity_gate_never_trips() {
        let ent = UnlimitedEntitlements;
        assert!(require_agent_capacity(&ent, OrgId::new(), 10_000).is_ok());
    }

    #[test]
    fn capped_capacity_gate_denies_at_ceiling() {
        let ent = CappedEntitlements { max: 1 };
        let result = require_agent_capacity(&ent, OrgId::new(), 1);
        assert!(matches!(
            result,
            Err(LicenseError::AgentLimitReached { limit: 1 })
        ));
    }

    #[test]
    fn capped_capacity_gate_admits_below_ceiling() {
        let ent = CappedEntitlements { max: 3 };
        assert!(require_agent_capacity(&ent, OrgId::new(), 2).is_ok());
    }

    #[test]
    fn feature_gate_denies_under_restrictive_policy() {
        let ent = CappedEntitlements { max: 0 };
        let result = require_feature(&ent, OrgId::new(), Feature::Reserved);
        assert!(matches!(
            result,
            Err(LicenseError::FeatureNotLicensed {
                feature: Feature::Reserved
            })
        ));
    }
}
