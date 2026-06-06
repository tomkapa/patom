//! Scheduling-subsystem error type. CLAUDE.md §12 — one error per
//! module boundary so callers exhaustively match.

use thiserror::Error;

use crate::agents::{AgentId, AgentStoreError};
use crate::runtime::PromptError;
use crate::types::ParseError;

use super::types::ScheduledTaskId;

/// All failure modes of the scheduling subsystem.
#[derive(Debug, Error)]
pub enum ScheduledTaskError {
    /// Row is absent or belongs to a different agent. The cross-owner
    /// case folds into NotFound by design: the cancel/list tools must
    /// not leak the existence of other agents' rows.
    #[error("scheduled task {0:?} not found")]
    NotFound(ScheduledTaskId),

    /// Per-agent task cap reached (CLAUDE.md §5). The model sees this
    /// at the `schedule_task` boundary and learns to ask the user to
    /// cancel something first.
    #[error("agent {agent:?} has hit the scheduled-task cap of {max}")]
    PerAgentCapExceeded { agent: AgentId, max: usize },

    /// Boundary parse failure (name, prompt, weekday set, time, tz).
    #[error("parse: {0}")]
    Parse(#[from] ParseError),

    /// Validation of the `agents` row failed (the owner agent referenced
    /// in the task does not exist any more).
    #[error("agent lookup: {0}")]
    Agent(#[from] AgentStoreError),

    /// The scheduler tick failed to enqueue a fire into the prompt queue.
    /// Distinct from `Db` so dashboards can separate queue-side faults
    /// from store-side ones.
    #[error("queue enqueue: {0}")]
    Queue(#[from] PromptError),

    #[error("schedule decode: {0}")]
    Decode(#[from] serde_json::Error),

    #[error("scheduled task store db error: {0}")]
    Db(#[from] sqlx::Error),

    /// Infrastructure error not covered by the typed variants — used
    /// for `begin_as_user` failures (auth store unreachable) so the
    /// `_for_user` paths can surface them without a fresh enum
    /// dependency on `AuthError`.
    #[error("scheduled task store backend: {0}")]
    Backend(String),
}
