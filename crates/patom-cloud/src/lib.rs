//! Patom cloud — paid-tier / billing.
//!
//! Commercial code that is compiled **only** under `patom-server`'s `cloud`
//! feature, so the default OSS / self-host binary never links any of it. Core
//! defines the [`patom::entitlements::Entitlements`] seam and the permissive
//! [`patom::entitlements::UnlimitedEntitlements`] default; this crate provides
//! the cloud-tier impl that the cloud binary injects at its composition root.

use patom::auth::OrgId;
use patom::billing::GrantAmount;
use patom::entitlements::{AgentLimit, Entitlements, Feature};

/// The launch-period signup credit grant: $2 = 2,000,000 micro-USD (#154).
///
/// A new workspace is seeded with this much free platform credit so it can
/// start using AI without paying. It is a **launch promo** — when it ends this
/// drops to a path that returns `None` from [`Entitlements::signup_grant`],
/// while [`Entitlements::credit_gate_active`] stays `true` (new orgs then start
/// blocked at zero and must bring their own key or pay).
pub const SIGNUP_GRANT_MICROS: i64 = 2_000_000;

/// The cloud-tier entitlement policy.
///
/// Today it differs from the OSS default only on the credit bits: the free
/// credit gate is active and the signup grant fires. Agent-count / feature
/// limits stay unlimited until paid tiers land (#131/#133).
#[derive(Debug, Default)]
pub struct CloudEntitlements;

impl CloudEntitlements {
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

impl Entitlements for CloudEntitlements {
    fn agent_limit(&self, _org: OrgId) -> AgentLimit {
        AgentLimit::Unlimited
    }

    fn allows(&self, _org: OrgId, _feature: Feature) -> bool {
        true
    }

    fn credit_gate_active(&self, _org: OrgId) -> bool {
        true
    }

    fn signup_grant(&self, _org: OrgId) -> Option<GrantAmount> {
        // `SIGNUP_GRANT_MICROS` is a positive constant, so the parse always
        // succeeds; `.ok()` keeps this panic-free.
        GrantAmount::try_from(SIGNUP_GRANT_MICROS).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::{CloudEntitlements, SIGNUP_GRANT_MICROS};
    use patom::auth::OrgId;
    use patom::billing::GrantAmount;
    use patom::entitlements::Entitlements;

    #[test]
    fn cloud_activates_gate_and_fires_signup_grant() {
        let ent = CloudEntitlements::new();
        assert!(ent.credit_gate_active(OrgId::new()));
        assert_eq!(
            ent.signup_grant(OrgId::new()).map(GrantAmount::get),
            Some(SIGNUP_GRANT_MICROS)
        );
    }
}
