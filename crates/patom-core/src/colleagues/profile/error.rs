//! The one error type for the colleague-profile module boundary (CLAUDE.md §12).

use thiserror::Error;

use crate::provider::ProviderError;
use crate::types::ParseError;

use crate::colleagues::ColleagueId;

/// Every failure mode of the profile board. Callers exhaustively match.
#[derive(Debug, Error)]
pub enum ProfileError {
    /// No profile row for the given colleague within the caller's org.
    #[error("profile for colleague {0:?} not found")]
    NotFound(ColleagueId),

    /// `profile_write`'s `subject` resolved to a colleague outside the caller's
    /// org. The store check (not the tool) is the one place this rule cannot be
    /// sidestepped — mirrors `MemoryStoreError::SubjectNotInOrg`.
    #[error("subject {subject:?} is not a colleague in the caller's org")]
    SubjectNotInOrg { subject: ColleagueId },

    /// A decoded field violated its `TryFrom` bound — schema and code disagree.
    #[error("profile decode: {0}")]
    Parse(#[from] ParseError),

    /// Embedding the composed `profile_text` failed. The mutation aborts before
    /// the row lands, mirroring the agent-store discipline (CLAUDE.md §6).
    #[error("profile embed: {0}")]
    Embed(#[from] ProviderError),

    #[error("profile store db error: {0}")]
    Db(#[from] sqlx::Error),
}
