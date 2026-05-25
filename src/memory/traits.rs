use std::fmt;
use std::sync::Arc;

use async_trait::async_trait;
use thiserror::Error;

use crate::agents::AgentStoreError;
use crate::auth::{LanguageResolverError, RuleResolverError};
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

    #[error("language resolver: {0}")]
    Language(#[from] LanguageResolverError),

    #[error("rule resolver: {0}")]
    Rule(#[from] RuleResolverError),
}

/// Provides per-turn context to the agent. Returns the system prompt
/// for the active turn mode; the implementation selects the right
/// `<core>` block via `kind_payload.kind()` and composes it with the
/// agent's role + memory section.
///
/// `viewer` is the participant the worker is currently driving — for an
/// agent↔agent session it disambiguates which side's role prompt to
/// load.
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
        kind_payload: &RequestKindPayload,
    ) -> Result<Arc<str>, MemoryError>;
}

pub type SharedMemory = Arc<dyn Memory>;
