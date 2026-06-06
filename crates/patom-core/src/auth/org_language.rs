//! Per-agent language lookup used by the system-prompt assembler.
//!
//! Sits between two stores so the agent worker doesn't have to: given an
//! [`AgentId`], find the agent's `org_id` (via [`SharedAgentStore`]),
//! then read that org's `default_language` (via [`SharedUserStore`]).
//! A bounded TTL cache makes the hot path cheap — every Normal turn
//! calls [`OrgLanguageResolver::language_for_agent`] before rendering the
//! `<language>` tag, but `(agent → org_id)` and `(org → language)` change
//! rarely.
//!
//! Cache invalidation. The cache is keyed by [`AgentId`] (the only thing
//! the caller has). Org-language changes are rare (an admin clicking the
//! language switcher), so the PATCH route calls
//! [`OrgLanguageResolver::invalidate_all`] rather than scanning entries
//! for the matching org — simpler and bounded.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use thiserror::Error;

use crate::agents::{AgentId, AgentStoreError, SharedAgentStore};
use crate::cache::BoundedTtlCache;
use crate::clock::SharedClock;

use super::error::AuthError;
use super::language::Language;
use super::store::SharedUserStore;

/// Cache size for the agent → language map. Matches the per-org agent
/// budget the prompt cache uses (`AGENT_PROMPT_CACHE_CAP = 256`) so the
/// two grow in lockstep — every agent that has a cached prompt also has
/// a cached language.
const ORG_LANGUAGE_CACHE_CAP: usize = 256;

/// TTL on the cached language. Matches the prompt cache's TTL (60s) so
/// the two surfaces share one liveness window; the PATCH route forces an
/// immediate invalidation rather than waiting for natural expiry.
const ORG_LANGUAGE_CACHE_TTL: Duration = Duration::from_mins(1);

/// Resolver errors. Each variant maps cleanly onto a wire-visible failure
/// the agent worker can surface in its existing `MemoryError` chain.
#[derive(Debug, Error)]
pub enum LanguageResolverError {
    #[error("agent lookup failed: {0}")]
    Agent(#[from] AgentStoreError),
    #[error("org lookup failed: {0}")]
    Org(#[from] AuthError),
}

/// Per-agent language lookup. Trait so the agent worker depends on a
/// narrow interface and tests can swap in a fake without standing up a
/// database.
#[async_trait]
pub trait OrgLanguageResolver: std::fmt::Debug + Send + Sync + 'static {
    /// Return the language the given agent should respond in. Hits the
    /// cache first; on miss, reads `agents.org_id` then
    /// `organizations.default_language`.
    async fn language_for_agent(&self, agent: AgentId) -> Result<Language, LanguageResolverError>;

    /// Drop every cached entry. Called by the PATCH /me/org/language
    /// handler so a language switch takes effect on the next turn rather
    /// than at TTL expiry.
    fn invalidate_all(&self);
}

/// Cheap-clone handle. Wrap once in the composition root and share with
/// `AgentMemory` and the HTTP handler that mutates the language.
pub type SharedOrgLanguageResolver = Arc<dyn OrgLanguageResolver>;

/// Production [`OrgLanguageResolver`] backed by the agents store + the
/// user store. Cheap-clone — the inner [`BoundedTtlCache`] is itself an
/// `Arc`, so cloning shares the underlying state.
#[derive(Debug, Clone)]
pub struct PgOrgLanguageResolver {
    agents: SharedAgentStore,
    users: SharedUserStore,
    cache: BoundedTtlCache<AgentId, Language>,
}

impl PgOrgLanguageResolver {
    #[must_use]
    pub fn new(agents: SharedAgentStore, users: SharedUserStore, clock: SharedClock) -> Self {
        Self {
            agents,
            users,
            cache: BoundedTtlCache::new(
                ORG_LANGUAGE_CACHE_CAP,
                ORG_LANGUAGE_CACHE_TTL,
                clock,
                "OrgLanguageCache",
            ),
        }
    }
}

#[async_trait]
impl OrgLanguageResolver for PgOrgLanguageResolver {
    async fn language_for_agent(&self, agent: AgentId) -> Result<Language, LanguageResolverError> {
        let agents = self.agents.clone();
        let users = self.users.clone();
        self.cache
            .get_or_load(agent, || async move {
                let record = agents.read(agent).await?;
                let language = users.read_org_language(record.org_id).await?;
                Ok::<Language, LanguageResolverError>(language)
            })
            .await
    }

    fn invalidate_all(&self) {
        self.cache.clear();
    }
}
