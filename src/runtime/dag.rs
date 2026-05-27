//! DAG turn budget — trait surface and Postgres impl.
//!
//! A DAG is the causal tree of conversations rooted at a single human
//! request. `prompt_request_dags` records `(root_request_id, turns_used,
//! turns_cap)` for that tree. `send_message` atomically increments
//! `turns_used` via [`DagBudget::bump_or_fail`]; when the budget is empty
//! the call fails and the offending row stays so callers see exactly which
//! message broke the cap. This is how we cap runaway agent-to-agent loops
//! at `MAX_DAG_TURNS` per root.
//!
//! [`DagBudget::quiescent`] is the terminal-emission helper: a DAG with no
//! `pending` or `processing` rows is finished, so the worker that flips
//! the last row to `done`/`failed` can publish a final `Done` chunk on the
//! root stream and close it.
//!
//! The queue's `enqueue` seeds new DAG rows inline (atomic with the first
//! request insert); this trait owns the read/update ops that
//! `send_message` and the quiescence trigger call directly.

use std::fmt;
use std::sync::Arc;

use async_trait::async_trait;
use sqlx::PgPool;

use crate::auth::UserId;

use super::error::PromptError;
use super::types::PromptRequestId;

/// Outcome of a successful budget bump.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BudgetBumped {
    pub turns_used: u32,
    pub turns_cap: u32,
}

/// Operations callers need on `prompt_request_dags`.
#[async_trait]
pub trait DagBudget: fmt::Debug + Send + Sync {
    /// Atomically increment `turns_used` for `root` if there is budget
    /// left. Returns the post-increment values on success, or
    /// [`PromptError::DagBudgetExceeded`] when the row's `turns_used`
    /// already equals `turns_cap`. The bump is intentionally not paired
    /// with a transactional rollback at the call site: `send_message`
    /// surfaces the rejection to the model and leaves the offending row
    /// in place so callers can see which message broke the cap.
    async fn bump_or_fail(&self, root: PromptRequestId) -> Result<BudgetBumped, PromptError>;

    /// Tenant-scoped variant of [`Self::bump_or_fail`]. Opens
    /// `begin_as_user(acting_user_id)` so the UPDATE on
    /// `prompt_request_dags` is filtered by RLS to rows the acting
    /// principal can see — a cross-tenant bump silently no-ops and
    /// surfaces as `DagNotFound` rather than touching another org's
    /// counter.
    async fn bump_or_fail_for_user(
        &self,
        acting_user_id: UserId,
        root: PromptRequestId,
    ) -> Result<BudgetBumped, PromptError>;

    /// True iff the DAG rooted at `root` has no live `prompt_requests` rows
    /// (no `pending` or `processing`). Used by the worker after a
    /// `mark_done` / `mark_failed` to decide whether to emit the final `Done`
    /// chunk on the root stream.
    async fn quiescent(&self, root: PromptRequestId) -> Result<bool, PromptError>;
}

/// Cheap-clone handle held by `send_message` and the quiescence trigger.
pub type SharedDagBudget = Arc<dyn DagBudget>;

/// Postgres-backed [`DagBudget`].
pub struct PgDagBudget {
    pool: PgPool,
}

impl PgDagBudget {
    #[must_use]
    pub const fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

impl fmt::Debug for PgDagBudget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PgDagBudget").finish_non_exhaustive()
    }
}

#[async_trait]
impl DagBudget for PgDagBudget {
    #[tracing::instrument(
        skip_all,
        name = "dag.bump",
        fields(
            patom.dag.root = %root,
            patom.dag.bump.outcome = tracing::field::Empty,
            patom.dag.turns_used = tracing::field::Empty,
            patom.dag.turns_cap = tracing::field::Empty,
        ),
    )]
    async fn bump_or_fail(&self, root: PromptRequestId) -> Result<BudgetBumped, PromptError> {
        let tx = crate::auth::begin_privileged(&self.pool).await?;
        bump_or_fail_in_tx(tx, root).await
    }

    #[tracing::instrument(
        skip_all,
        name = "dag.bump",
        fields(
            patom.dag.root = %root,
            patom.dag.bump.outcome = tracing::field::Empty,
            patom.dag.turns_used = tracing::field::Empty,
            patom.dag.turns_cap = tracing::field::Empty,
        ),
    )]
    async fn bump_or_fail_for_user(
        &self,
        acting_user_id: UserId,
        root: PromptRequestId,
    ) -> Result<BudgetBumped, PromptError> {
        let tx = crate::auth::begin_as_user(&self.pool, acting_user_id).await?;
        bump_or_fail_in_tx(tx, root).await
    }

    #[tracing::instrument(
        skip_all,
        name = "dag.quiescent_check",
        fields(
            patom.dag.root = %root,
            patom.dag.quiescent = tracing::field::Empty,
        ),
    )]
    async fn quiescent(&self, root: PromptRequestId) -> Result<bool, PromptError> {
        // Privileged: worker-side check; cross-tenant infrastructure
        // read. Same reasoning as `bump_or_fail`.
        let mut tx = crate::auth::begin_privileged(&self.pool).await?;
        // EXISTS short-circuits on the first live row so the cost is
        // bounded by index lookup, not the DAG's size.
        let (live,): (bool,) = sqlx::query_as(
            "SELECT EXISTS(
                SELECT 1 FROM prompt_requests
                WHERE root_request_id = $1
                  AND status IN ('pending','processing')
             )",
        )
        .bind(root)
        .fetch_one(&mut *tx)
        .await?;
        tx.commit().await?;
        tracing::Span::current().record("patom.dag.quiescent", !live);
        Ok(!live)
    }
}

/// Body of `bump_or_fail` / `bump_or_fail_for_user`. Takes the
/// opened transaction by value (privileged or `begin_as_user`) and
/// commits before returning so the same SQL lives in one place across
/// both entry points.
async fn bump_or_fail_in_tx(
    mut tx: sqlx::Transaction<'_, sqlx::Postgres>,
    root: PromptRequestId,
) -> Result<BudgetBumped, PromptError> {
    let row: Option<(i64, i64)> = sqlx::query_as(
        "UPDATE prompt_request_dags
                SET turns_used = turns_used + 1
              WHERE root_request_id = $1
                AND turns_used < turns_cap
              RETURNING turns_used, turns_cap",
    )
    .bind(root)
    .fetch_optional(&mut *tx)
    .await?;

    let span = tracing::Span::current();
    if let Some((used, cap)) = row {
        tx.commit().await?;
        let bumped = BudgetBumped {
            turns_used: u32_or_invariant(used, "turns_used"),
            turns_cap: u32_or_invariant(cap, "turns_cap"),
        };
        span.record("patom.dag.bump.outcome", "ok");
        span.record("patom.dag.turns_used", bumped.turns_used);
        span.record("patom.dag.turns_cap", bumped.turns_cap);
        return Ok(bumped);
    }

    // The bump did not match. Either the row is missing (caller
    // error) or it is at its cap (budget exhausted). Read the row to
    // disambiguate.
    let exists: Option<(i64, i64)> = sqlx::query_as(
        "SELECT turns_used, turns_cap FROM prompt_request_dags WHERE root_request_id = $1",
    )
    .bind(root)
    .fetch_optional(&mut *tx)
    .await?;
    tx.commit().await?;

    if let Some((used, cap)) = exists {
        let used = u32_or_invariant(used, "turns_used");
        let cap = u32_or_invariant(cap, "turns_cap");
        span.record("patom.dag.bump.outcome", "exceeded");
        span.record("patom.dag.turns_used", used);
        span.record("patom.dag.turns_cap", cap);
        Err(PromptError::DagBudgetExceeded {
            root,
            turns_used: used,
            turns_cap: cap,
        })
    } else {
        span.record("patom.dag.bump.outcome", "not_found");
        Err(PromptError::DagNotFound(root))
    }
}

/// Decode a `BIGINT` column known by construction to fit in `u32`. The
/// `prompt_request_dags` `turns_*` columns hold counters that never approach
/// `i64::MAX`; observing a negative or overflowed value is schema corruption
/// (CLAUDE.md §6).
fn u32_or_invariant(raw: i64, name: &'static str) -> u32 {
    u32::try_from(raw).unwrap_or_else(|_| panic!("invariant: {name} fits in u32, got {raw}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn u32_or_invariant_passes_through_in_range() {
        assert_eq!(u32_or_invariant(0, "x"), 0);
        assert_eq!(u32_or_invariant(64, "x"), 64);
        assert_eq!(u32_or_invariant(i64::from(u32::MAX), "x"), u32::MAX);
    }

    #[test]
    #[should_panic(expected = "invariant: x fits in u32")]
    fn u32_or_invariant_panics_on_overflow() {
        // i64::MAX cannot fit in u32; observing this means schema corruption.
        u32_or_invariant(i64::MAX, "x");
    }

    #[test]
    #[should_panic(expected = "invariant: x fits in u32")]
    fn u32_or_invariant_panics_on_negative() {
        u32_or_invariant(-1, "x");
    }

    #[test]
    fn budget_bumped_is_value_typed() {
        // Sanity — derives Eq/Copy so callers can compare and stash without
        // ceremony. If a future field breaks Copy this test fails to compile.
        let a = BudgetBumped {
            turns_used: 1,
            turns_cap: 64,
        };
        let b = a;
        assert_eq!(a, b);
    }
}
