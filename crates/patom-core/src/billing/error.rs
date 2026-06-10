//! The budget module's one error type (CLAUDE.md §12).

use thiserror::Error;

use crate::auth::OrgId;

/// Every failure the budget gate or settle path can surface.
#[derive(Debug, Error)]
pub enum BillingError {
    /// The org has spent its monthly cap. Carries the period total and the cap
    /// (both micro-USD) so the caller — and the `429` / SSE failure it maps to
    /// — can report exactly how far over the line the request was.
    #[error("org {org} budget exceeded: {used_micro_usd}/{cap_micro_usd} micro-USD this period")]
    Exceeded {
        org: OrgId,
        used_micro_usd: i64,
        cap_micro_usd: i64,
    },

    /// The org's free-credit balance is exhausted (#154) and the credit gate is
    /// active for it. Carries the (signed) balance so the caller — and the `402`
    /// it maps to — can report the shortfall. Distinct from [`Self::Exceeded`]:
    /// a zero balance means "out of platform credit" (top up / bring your own
    /// key), not "over a configured monthly cap".
    #[error("org {org} is out of credit: balance {balance_micro_usd} micro-USD")]
    OutOfCredit { org: OrgId, balance_micro_usd: i64 },

    /// A Postgres failure on a gate read or the settle upsert.
    #[error(transparent)]
    Db(#[from] sqlx::Error),
}
