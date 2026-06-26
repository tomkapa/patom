//! Persistence seam for `turn_metrics` rows.

use std::fmt;
use std::sync::Arc;

use async_trait::async_trait;

use crate::threads::AgentThreadId;

use super::error::TurnRecorderError;
use super::types::TurnMetricsRow;

#[async_trait]
pub trait TurnMetricsStore: fmt::Debug + Send + Sync {
    /// Insert one row. Best-effort: callers `tracing::error!` on failure
    /// and continue (CLAUDE.md §6 — observability never blocks the
    /// user-visible turn).
    async fn record(&self, row: TurnMetricsRow) -> Result<(), TurnRecorderError>;

    /// The provider-reported `input_tokens` of the most recent *full-prompt*
    /// turn for `state_id` — every [`crate::runtime::MetricKind`] except
    /// `Compaction` (fold sub-calls carry a chunk, not the assembled prompt).
    ///
    /// `None` when the agent has never completed a turn in this thread. The
    /// compaction trigger (#182) prefers this real count over the crude
    /// `chars/4` estimate, which overcounts tool/JSON-heavy content and fires
    /// compaction far too early.
    async fn latest_full_prompt_input_tokens(
        &self,
        state_id: AgentThreadId,
    ) -> Result<Option<u32>, TurnRecorderError>;
}

pub type SharedTurnMetricsStore = Arc<dyn TurnMetricsStore>;

#[cfg(test)]
pub use in_memory::InMemoryTurnMetricsStore;

#[cfg(test)]
mod in_memory {
    use std::sync::Mutex;

    use super::super::error::TurnRecorderError;
    use super::super::types::TurnMetricsRow;
    use super::TurnMetricsStore;

    /// Test double — append-only Vec. Mirrors the in-memory fake pattern in
    /// [`crate::tools::recorder`].
    pub struct InMemoryTurnMetricsStore {
        rows: Mutex<Vec<TurnMetricsRow>>,
    }

    impl InMemoryTurnMetricsStore {
        #[must_use]
        pub fn new() -> Self {
            Self {
                rows: Mutex::new(Vec::new()),
            }
        }

        #[must_use]
        pub fn snapshot(&self) -> Vec<TurnMetricsRow> {
            self.rows
                .lock()
                .expect("invariant: lock not poisoned")
                .clone()
        }
    }

    impl Default for InMemoryTurnMetricsStore {
        fn default() -> Self {
            Self::new()
        }
    }

    impl std::fmt::Debug for InMemoryTurnMetricsStore {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("InMemoryTurnMetricsStore")
                .finish_non_exhaustive()
        }
    }

    #[async_trait::async_trait]
    impl TurnMetricsStore for InMemoryTurnMetricsStore {
        async fn record(&self, row: TurnMetricsRow) -> Result<(), TurnRecorderError> {
            self.rows
                .lock()
                .expect("invariant: lock not poisoned")
                .push(row);
            Ok(())
        }

        async fn latest_full_prompt_input_tokens(
            &self,
            state_id: super::AgentThreadId,
        ) -> Result<Option<u32>, TurnRecorderError> {
            let rows = self.rows.lock().expect("invariant: lock not poisoned");
            let latest = rows
                .iter()
                .filter(|r| r.state_id == state_id && r.kind.as_str() != "compaction")
                .max_by_key(|r| r.started_at)
                .map(|r| u32::try_from(r.input_tokens.get()).unwrap_or(0));
            Ok(latest)
        }
    }
}
