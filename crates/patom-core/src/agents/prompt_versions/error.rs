use thiserror::Error;

use crate::agents::AgentId;
use crate::types::ParseError;

use super::types::PromptVersionNumber;

/// Per-module error envelope (CLAUDE.md §12). Every public method on
/// [`super::PromptVersionStore`] returns this so callers branch
/// exhaustively at the seam.
#[derive(Debug, Error)]
pub enum PromptVersionError {
    /// No history rows for this agent yet (e.g. seed migration didn't
    /// run). Operational fault — surfaces as a 500.
    #[error("agent {0} has no prompt versions on file")]
    NoVersionsForAgent(AgentId),

    /// Restore target agent was removed (or visibility flipped) between
    /// the visibility gate and the FOR-UPDATE lock. 404.
    #[error("agent {0} not found")]
    AgentNotFound(AgentId),

    /// Restore target version doesn't exist for this agent. 404.
    #[error("agent {agent} has no version {version:?}")]
    VersionNotFound {
        agent: AgentId,
        version: PromptVersionNumber,
    },

    #[error("invalid stored row: {0}")]
    Parse(#[from] ParseError),

    #[error(transparent)]
    Db(#[from] sqlx::Error),
}
