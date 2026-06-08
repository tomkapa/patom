//! The one error type for the colleagues module boundary (CLAUDE.md §12).

use thiserror::Error;

use crate::types::ParseError;

use super::types::{ColleagueId, ColleagueKind};

/// Every failure mode of the colleagues subsystem. Callers exhaustively match.
#[derive(Debug, Error)]
pub enum ColleagueError {
    /// No colleague row for the given id within the caller's org (RLS-scoped).
    #[error("colleague {0:?} not found")]
    NotFound(ColleagueId),

    /// A decoded row violates the kind ⇔ satellite invariant — `Human` without
    /// a `user_id`, `Agent` without an `agent_id`, or a satellite of the wrong
    /// kind present. The `colleagues_kind_satellite` column `CHECK` should make
    /// this unobservable; seeing it means schema and code disagree (§6).
    #[error("invariant: colleague kind/satellite mismatch")]
    KindSatelliteMismatch,

    /// A satellite (`user_id` / `agent_id`) had no colleague row in the org —
    /// the directory mint seam failed to run for that human or agent.
    #[error("no colleague mapped for {kind:?} satellite")]
    SatelliteUnmapped { kind: ColleagueKind },

    #[error("colleague decode: {0}")]
    Parse(#[from] ParseError),

    #[error("colleague store db error: {0}")]
    Db(#[from] sqlx::Error),
}
