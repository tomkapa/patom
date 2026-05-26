use thiserror::Error;

use crate::types::ParseError;

/// Per-module error envelope (CLAUDE.md §12).
#[derive(Debug, Error)]
pub enum TurnRecorderError {
    #[error(transparent)]
    Db(#[from] sqlx::Error),

    #[error(transparent)]
    Parse(#[from] ParseError),
}
