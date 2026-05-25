//! Per-org rule directive shared across every agent the org owns.
//!
//! Two pieces live here:
//!
//! 1. The [`OrganizationRule`] newtype — a length-capped, cheap-clone
//!    body parsed at the boundary (`TryFrom<String>`). Wraps `Arc<str>`
//!    so the resolver cache can hand out clones without copying the
//!    underlying buffer.
//!
//! 2. The [`OrgRuleResolver`] trait + [`PgOrgRuleResolver`] impl. Given
//!    an [`AgentId`], the resolver finds the agent's `org_id` and reads
//!    that org's `default_rule`, returning `None` when the org has not
//!    configured a rule. A bounded TTL cache makes the hot path cheap —
//!    every agent turn calls [`OrgRuleResolver::rule_for_agent`] before
//!    rendering the system prompt, but `(agent → org_id)` and
//!    `(org → rule)` change rarely. Mirrors the structure of
//!    [`super::org_language`] one-for-one; CLAUDE.md §4 says three
//!    occurrences before abstracting, and two parallel resolvers do not
//!    yet justify a generic.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use thiserror::Error;

use crate::agents::{AgentId, AgentStoreError, SharedAgentStore};
use crate::cache::BoundedTtlCache;
use crate::clock::SharedClock;
use crate::types::ParseError;

use super::error::AuthError;
use super::limits::MAX_ORG_RULE_BYTES;
use super::store::SharedUserStore;

/// A length-capped, cheap-clone organization rule body.
///
/// The smart constructor [`OrganizationRule::try_from`] is the only way
/// in (CLAUDE.md §1: parse, don't validate). It rejects empty / whitespace
/// strings and anything past [`MAX_ORG_RULE_BYTES`]. Wraps `Arc<str>` so
/// the per-agent resolver cache can return the value by clone without
/// copying — the body sits behind the same `<organization-rule>` tag in
/// every system prompt the org emits, so a single allocation is shared
/// across all rendered prompts.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct OrganizationRule(Arc<str>);

impl OrganizationRule {
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl TryFrom<String> for OrganizationRule {
    type Error = ParseError;

    fn try_from(raw: String) -> Result<Self, Self::Error> {
        // Trim once and treat the trimmed value as canonical:
        //
        // * Reject pure-whitespace input — a blank rule rendered into
        //   the system prompt is worse than no rule at all (the model
        //   would see an empty `<organization-rule>` block and treat
        //   the surrounding scaffolding as meaningful by accident).
        // * The cap applies to the trimmed length, so a paste with
        //   leading/trailing newlines doesn't eat into the budget.
        // * The stored body is the trimmed slice — whitespace can't
        //   bleed into the cached prefix and a FE round-trip that
        //   normalizes whitespace (CRLF↔LF, trailing-newline strip)
        //   produces a byte-identical prefix to the original save.
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Err(ParseError::Empty {
                field: "organization_rule",
            });
        }
        if trimmed.len() > MAX_ORG_RULE_BYTES {
            return Err(ParseError::TooLong {
                field: "organization_rule",
                max: MAX_ORG_RULE_BYTES,
                got: trimmed.len(),
            });
        }
        Ok(Self(Arc::from(trimmed)))
    }
}

impl TryFrom<&str> for OrganizationRule {
    type Error = ParseError;

    fn try_from(raw: &str) -> Result<Self, Self::Error> {
        Self::try_from(raw.to_string())
    }
}

impl std::fmt::Debug for OrganizationRule {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Debug logs the byte length only — the rule body may contain
        // sensitive operational guidance that should not land in a stray
        // tracing event.
        f.debug_tuple("OrganizationRule")
            .field(&self.0.len())
            .finish()
    }
}

/// Cache size for the agent → rule map. Matches the prompt cache cap
/// (`AGENT_PROMPT_CACHE_CAP = 256`) so the two surfaces grow in lockstep:
/// every agent that has a cached prompt also has a cached rule.
const ORG_RULE_CACHE_CAP: usize = 256;

/// TTL on the cached rule. Matches the prompt + language caches (60s) so
/// the three surfaces share one liveness window; the PATCH route forces
/// an immediate invalidation rather than waiting for natural expiry.
const ORG_RULE_CACHE_TTL: Duration = Duration::from_secs(60);

/// Resolver errors.
///
/// Each variant maps cleanly onto a wire-visible failure the agent worker
/// surfaces in its existing `MemoryError` chain — same pattern as
/// [`super::org_language::LanguageResolverError`].
#[derive(Debug, Error)]
pub enum RuleResolverError {
    #[error("agent lookup failed: {0}")]
    Agent(#[from] AgentStoreError),
    #[error("org lookup failed: {0}")]
    Org(#[from] AuthError),
}

/// Per-agent rule lookup. Trait so the agent worker depends on a narrow
/// interface and tests can swap in a fake without standing up a database.
#[async_trait]
pub trait OrgRuleResolver: std::fmt::Debug + Send + Sync + 'static {
    /// Return the rule the given agent's org has configured, or `None`
    /// when the org has no rule. Hits the cache first; on miss, reads
    /// `agents.org_id` then `organizations.default_rule`.
    async fn rule_for_agent(
        &self,
        agent: AgentId,
    ) -> Result<Option<OrganizationRule>, RuleResolverError>;

    /// Drop every cached entry. Called by the PATCH /me/org/rule
    /// handler so a rule edit takes effect on the next turn rather than
    /// at TTL expiry.
    fn invalidate_all(&self);
}

/// Cheap-clone handle. Wrap once in the composition root and share with
/// `AgentMemory` and the HTTP handler that mutates the rule.
pub type SharedOrgRuleResolver = Arc<dyn OrgRuleResolver>;

/// Production [`OrgRuleResolver`] backed by the agents store + the user
/// store. Cheap-clone — the inner [`BoundedTtlCache`] is itself an
/// `Arc`, so cloning shares the underlying state.
#[derive(Debug, Clone)]
pub struct PgOrgRuleResolver {
    agents: SharedAgentStore,
    users: SharedUserStore,
    cache: BoundedTtlCache<AgentId, Option<OrganizationRule>>,
}

impl PgOrgRuleResolver {
    #[must_use]
    pub fn new(agents: SharedAgentStore, users: SharedUserStore, clock: SharedClock) -> Self {
        Self {
            agents,
            users,
            cache: BoundedTtlCache::new(
                ORG_RULE_CACHE_CAP,
                ORG_RULE_CACHE_TTL,
                clock,
                "OrgRuleCache",
            ),
        }
    }
}

#[async_trait]
impl OrgRuleResolver for PgOrgRuleResolver {
    async fn rule_for_agent(
        &self,
        agent: AgentId,
    ) -> Result<Option<OrganizationRule>, RuleResolverError> {
        let agents = self.agents.clone();
        let users = self.users.clone();
        self.cache
            .get_or_load(agent, || async move {
                let record = agents.read(agent).await?;
                let rule = users.read_org_rule(record.org_id).await?;
                Ok::<Option<OrganizationRule>, RuleResolverError>(rule)
            })
            .await
    }

    fn invalidate_all(&self) {
        self.cache.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_empty_and_whitespace() {
        assert!(OrganizationRule::try_from("").is_err());
        assert!(OrganizationRule::try_from("   \n\t").is_err());
    }

    #[test]
    fn rejects_oversize() {
        let big = "a".repeat(MAX_ORG_RULE_BYTES + 1);
        assert!(OrganizationRule::try_from(big).is_err());
    }

    #[test]
    fn accepts_at_cap() {
        let at_cap = "a".repeat(MAX_ORG_RULE_BYTES);
        let r = OrganizationRule::try_from(at_cap).expect("at cap is OK");
        assert_eq!(r.len(), MAX_ORG_RULE_BYTES);
    }

    #[test]
    fn round_trips_body() {
        let body = "Be terse. Cite file:line.";
        let r = OrganizationRule::try_from(body).expect("valid");
        assert_eq!(r.as_str(), body);
    }

    #[test]
    fn trims_leading_and_trailing_whitespace() {
        // Leading/trailing whitespace is stripped before the body is
        // stored, so the cached prefix stays byte-stable across FE
        // round-trips that normalize CRLF / trim trailing newlines.
        let r = OrganizationRule::try_from("\n\n  Be terse.  \n").expect("valid");
        assert_eq!(r.as_str(), "Be terse.");
    }

    #[test]
    fn cap_applies_to_trimmed_length() {
        // The byte cap is on the trimmed body — a paste with leading /
        // trailing whitespace can't eat into the budget, and a body of
        // exactly MAX bytes surrounded by whitespace is still accepted.
        let mut raw = String::with_capacity(MAX_ORG_RULE_BYTES + 4);
        raw.push_str("  ");
        raw.push_str(&"a".repeat(MAX_ORG_RULE_BYTES));
        raw.push_str("\n\n");
        let r = OrganizationRule::try_from(raw).expect("trimmed body fits");
        assert_eq!(r.len(), MAX_ORG_RULE_BYTES);
    }
}
