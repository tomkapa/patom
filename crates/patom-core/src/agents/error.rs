use thiserror::Error;

use crate::types::ParseError;

use super::types::{AgentId, AgentName};

/// All failure modes of the agents subsystem. CLAUDE.md §12: one error per module
/// boundary so callers exhaustively match.
#[derive(Debug, Error)]
pub enum AgentStoreError {
    #[error("agent {0:?} not found")]
    NotFound(AgentId),

    /// Case-insensitive name lookup miss. Surfaces to the model when it
    /// tries to `send_message` a peer that does not exist.
    #[error("agent named {0:?} not found")]
    NameNotFound(AgentName),

    /// Create attempted with a name already in use (case-insensitive). The
    /// `agents_name_lower_unique` index is the source of truth — Postgres
    /// raises a unique-violation (SQLSTATE 23505) which the store maps to
    /// this variant so callers can surface a clean "name taken" message
    /// instead of an opaque DB error.
    #[error("agent name {0:?} is already taken")]
    NameTaken(AgentName),

    /// `default_id()` was called before the init seeder ran. A correctly composed
    /// `Server` always seeds before exposing the store, so this is a programmer
    /// error in test setup or a misordered startup, not a runtime condition.
    #[error("no default agent has been seeded")]
    NoDefault,

    /// Caller tried to delete the row currently flagged `is_default = TRUE`. The
    /// HTTP layer and the worker both assume `default_id()` always resolves, so
    /// removing the default is unsafe — promote another row first.
    #[error("cannot delete the default agent")]
    DefaultDeletionForbidden,

    /// Caller tried to delete a row referenced by at least one session. The FK
    /// `sessions.agent_id REFERENCES agents(id)` is `ON DELETE RESTRICT` so the
    /// session history of agents that ever existed is preserved.
    #[error("agent {0:?} is referenced by one or more sessions")]
    InUse(AgentId),

    #[error("agent record decode: {0}")]
    Parse(#[from] ParseError),

    #[error("agent store backend error: {0}")]
    Backend(String),

    #[error("agent store db error: {0}")]
    Db(#[from] sqlx::Error),
}
