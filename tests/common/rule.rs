//! Test-only [`OrgRuleResolver`] that returns a fixed rule (or `None`).
//!
//! Mirrors [`super::lang::StaticOrgLanguageResolver`] for the per-org
//! rule resolver — most tests only care about whether the
//! `<organization-rule>` tag appears in the rendered prompt, not how
//! the value was resolved. Constructed with a starting value;
//! [`StaticOrgRuleResolver::set`] swaps it at runtime so invalidation
//! paths can be exercised.

use std::sync::Arc;
use std::sync::Mutex;

use async_trait::async_trait;

use patom::agents::AgentId;
use patom::auth::{OrgRuleResolver, OrganizationRule, RuleResolverError, SharedOrgRuleResolver};

#[derive(Debug)]
pub struct StaticOrgRuleResolver {
    rule: Mutex<Option<OrganizationRule>>,
    invalidations: Mutex<usize>,
}

impl StaticOrgRuleResolver {
    pub fn new(rule: Option<OrganizationRule>) -> Self {
        Self {
            rule: Mutex::new(rule),
            invalidations: Mutex::new(0),
        }
    }

    #[allow(dead_code)]
    pub fn set(&self, rule: Option<OrganizationRule>) {
        *self.rule.lock().expect("lock") = rule;
    }

    #[allow(dead_code)]
    pub fn invalidations(&self) -> usize {
        *self.invalidations.lock().expect("lock")
    }
}

#[async_trait]
impl OrgRuleResolver for StaticOrgRuleResolver {
    async fn rule_for_agent(
        &self,
        _agent: AgentId,
    ) -> Result<Option<OrganizationRule>, RuleResolverError> {
        Ok(self.rule.lock().expect("lock").clone())
    }

    fn invalidate_all(&self) {
        *self.invalidations.lock().expect("lock") += 1;
    }
}

/// Convenience: empty (no-rule) resolver wrapped in the trait handle
/// the composition root uses. Tests that wire a full `AppState` reach
/// for this when they don't care about the rule.
pub fn empty_resolver() -> SharedOrgRuleResolver {
    Arc::new(StaticOrgRuleResolver::new(None))
}
