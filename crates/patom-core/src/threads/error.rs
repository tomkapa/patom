use thiserror::Error;

use super::traits::ThreadId;

/// One error type per module boundary (CLAUDE.md §12).
#[derive(Debug, Error)]
pub enum ThreadError {
    #[error("thread {0:?} not found")]
    NotFound(ThreadId),

    #[error("thread store backend error: {0}")]
    Backend(String),

    #[error("thread store db error: {0}")]
    Db(#[from] sqlx::Error),
}
