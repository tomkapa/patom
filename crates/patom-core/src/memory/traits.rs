use std::fmt;
use std::sync::Arc;

use async_trait::async_trait;
use thiserror::Error;

use crate::agents::AgentStoreError;
use crate::auth::{LanguageResolverError, RuleResolverError};
use crate::colleagues::ColleagueError;
use crate::runtime::RequestKindPayload;
use crate::session::{SessionError, SessionId};
use crate::types::Participant;

#[derive(Debug, Error)]
pub enum MemoryError {
    #[error("memory backend error: {0}")]
    Backend(String),

    #[error("session lookup: {0}")]
    Session(#[from] SessionError),

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
///
/// `viewer` is the participant the worker is currently driving — for an
/// agent↔agent session it disambiguates which side's role prompt to
/// load.
///
/// `counterpart` is the *other* end of the 2-party session — the colleague the
/// agent is addressing this turn. It lets the prompt name who's speaking (with
/// their colleague id) so the model uses the right `subject`/recipient instead
/// of guessing from the roster, which is ambiguous once the org has more than
/// one human. A `System` counterpart (reflection/resolution) yields no line.
///
/// `kind_payload` mirrors `prompt_requests.kind_payload` so kind-specific
/// composition (e.g. Resolution reserving `M-1` / `M-2` for the flagged
/// pair) reads from the same source the tool-call path does.
#[async_trait]
pub trait Memory: Send + Sync + fmt::Debug {
    async fn system_prompt(
        &self,
        session: SessionId,
        viewer: Participant,
        counterpart: Participant,
        kind_payload: &RequestKindPayload,
    ) -> Result<Arc<str>, MemoryError>;

    /// System prompt for a thread-feed turn (the read-at-run chat path).
    ///
    /// Unlike [`Self::system_prompt`], a thread turn has no single
    /// `counterpart` (the feed is multi-party) and no `SessionId` (the
    /// participation id is the turn scope). The agent's role + stable memory +
    /// org roster + language + rule are composed from `viewer` alone. The
    /// session-scoped contextual-memory layer (top-K retrieval keyed on the
    /// opening message) degrades to empty here until it is rehomed onto the
    /// thread feed — it is enrichment, never load-bearing (doc/memory.md §1.3).
    async fn system_prompt_for_thread(
        &self,
        viewer: Participant,
        kind_payload: &RequestKindPayload,
    ) -> Result<Arc<str>, MemoryError>;
}

pub type SharedMemory = Arc<dyn Memory>;
