//! Per-LLM-call audit metrics (`turn_metrics`).
//!
//! Recorded immediately after `call_provider` returns so token cost, latency,
//! stop reason, and context size land in a columnar table the Logs & Metrics
//! tab aggregates over in SQL (doc/logs_metrics_tab.md §4.2). One INSERT per
//! turn; ~80 bytes per row.

mod error;
mod pg_store;
mod store;
mod types;

pub use error::TurnRecorderError;
pub use pg_store::PgTurnMetricsStore;
#[cfg(test)]
pub use store::InMemoryTurnMetricsStore;
pub use store::{SharedTurnMetricsStore, TurnMetricsStore};
pub use types::{
    DurationMs, HistoryCount, InputTokens, OutputTokens, StopReasonLabel, TurnMetricsId,
    TurnMetricsRow,
};
