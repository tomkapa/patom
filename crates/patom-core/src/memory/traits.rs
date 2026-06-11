use std::fmt;
use std::sync::Arc;

use async_trait::async_trait;
use thiserror::Error;

use crate::agents::AgentStoreError;
use crate::auth::{LanguageResolverError, RuleResolverError};
use crate::colleagues::ColleagueError;
use crate::runtime::RequestKindPayload;
use crate::threads::ThreadId;
use crate::types::Participant;

#[derive(Debug, Error)]
pub enum MemoryError {
    #[error("memory backend error: {0}")]
    Backend(String),

    #[error("agent lookup: {0}")]
    Agent(#[from] AgentStoreError),

    /// Resolving a `Collaborator` memory's subject colleague (or the roster
    /// used to hydrate subject names) failed.
    #[error("colleague lookup: {0}")]
    Colleague(#[from] ColleagueError),

    #[error("language resolver: {0}")]
    Language(#[from] LanguageResolverError),

    #[error("rule resolver: {0}")]
    Rule(#[from] RuleResolverError),
}

/// Provides per-turn context to the agent.
///
/// Returns the system prompt for the active turn mode; the implementation
/// selects the right `<core>` block via `kind_payload.kind()` and composes
/// it with the agent's role + memory section.
#[async_trait]
pub trait Memory: Send + Sync + fmt::Debug {
    /// System prompt for a thread-feed turn (the read-at-run chat path) and the
    /// background-cognition path.
    ///
    /// A thread turn has no single `counterpart` (the feed is multi-party) and
    /// no per-conversation scope id; the agent's role + stable memory + org
    /// roster + language + rule are composed from `viewer` alone. The
    /// session-scoped contextual-memory layer (top-K retrieval keyed on the
    /// opening message) degrades to empty here until it is rehomed onto the
    /// thread feed — it is enrichment, never load-bearing (doc/memory.md §1.3).
    /// `thread` is the turn's thread (`None` for the background-cognition
    /// path, which has no feed), used to resolve per-platform display
    /// labels for the roster (e.g. Slack handles in a Slack-rooted thread)
    /// without changing canonical colleague identity.
    async fn system_prompt_for_thread(
        &self,
        viewer: Participant,
        thread: Option<ThreadId>,
        kind_payload: &RequestKindPayload,
    ) -> Result<Arc<str>, MemoryError>;
}

pub type SharedMemory = Arc<dyn Memory>;
