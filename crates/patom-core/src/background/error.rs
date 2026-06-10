use thiserror::Error;

use super::traits::BackgroundTurnId;

/// One error type per module boundary (CLAUDE.md §12).
#[derive(Debug, Error)]
pub enum BackgroundError {
    #[error("background turn {0:?} not found")]
    NotFound(BackgroundTurnId),

    #[error("background store backend error: {0}")]
    Backend(String),

    #[error("background store db error: {0}")]
    Db(#[from] sqlx::Error),
}
