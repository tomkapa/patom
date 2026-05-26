//! Persistence seam for `turn_metrics` rows.

use std::fmt;
use std::sync::Arc;

use async_trait::async_trait;

use super::error::TurnRecorderError;
use super::types::TurnMetricsRow;

#[async_trait]
pub trait TurnMetricsStore: fmt::Debug + Send + Sync {
    /// Insert one row. Best-effort: callers `tracing::error!` on failure
    /// and continue (CLAUDE.md §6 — observability never blocks the
    /// user-visible turn).
    async fn record(&self, row: TurnMetricsRow) -> Result<(), TurnRecorderError>;
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
    }
}
