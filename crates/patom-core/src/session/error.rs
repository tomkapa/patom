use thiserror::Error;

use crate::agents::AgentId;

use super::traits::SessionId;

#[derive(Debug, Error)]
pub enum SessionError {
    #[error("session {0:?} not found")]
    NotFound(SessionId),

    /// Caller passed an `AgentId` that does not exist in the `agents` table.
    /// Distinct from [`SessionError::NotFound`] (about the session itself) so
    /// handlers can map it to a different status code.
    #[error("agent {0:?} not found")]
    AgentNotFound(AgentId),

    /// `resolve_or_create_for_pair` was called with `a == b`. A session whose
    /// participants are equal is representationally invalid (you can't talk to
    /// yourself). CLAUDE.md §1: parse, don't validate — caller checks before
    /// reaching the store.
    #[error("self-session forbidden: a == b")]
    SelfSession,

    #[error("session {id:?} would exceed message cap ({max})")]
    MessageCapExceeded { id: SessionId, max: u32 },

    #[error("session store backend error: {0}")]
    Backend(String),

    #[error("session store db error: {0}")]
    Db(#[from] sqlx::Error),
}
