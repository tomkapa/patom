//! The budget module's one error type (CLAUDE.md §12).

use thiserror::Error;

use crate::auth::OrgId;

/// Every failure the budget gate or settle path can surface.
#[derive(Debug, Error)]
pub enum BudgetError {
    /// The org has spent its monthly cap. Carries the period total and the cap
    /// (both micro-USD) so the caller — and the `429` / SSE failure it maps to
    /// — can report exactly how far over the line the request was.
    #[error("org {org} budget exceeded: {used_micro_usd}/{cap_micro_usd} micro-USD this period")]
    Exceeded {
        org: OrgId,
        used_micro_usd: i64,
        cap_micro_usd: i64,
    },

    /// A Postgres failure on a gate read or the settle upsert.
    #[error(transparent)]
    Db(#[from] sqlx::Error),
}
