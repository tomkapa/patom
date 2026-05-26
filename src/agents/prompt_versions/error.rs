use thiserror::Error;

use crate::agents::AgentId;
use crate::types::ParseError;

/// Per-module error envelope (CLAUDE.md §12). Every public method on
/// [`super::PromptVersionStore`] returns this so callers branch
/// exhaustively at the seam.
#[derive(Debug, Error)]
pub enum PromptVersionError {
    #[error("agent {0} has no prompt versions on file")]
    NoVersionsForAgent(AgentId),

    #[error("invalid stored row: {0}")]
    Parse(#[from] ParseError),

    #[error(transparent)]
    Db(#[from] sqlx::Error),
}
