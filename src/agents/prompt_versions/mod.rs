//! Append-only history of an agent's `(system_prompt, model)` tuple.
//!
//! Every `PATCH /agents/:id` that actually changes either field inserts a
//! new row into `agent_prompt_versions` in the same transaction as the
//! `agents` UPDATE. The Logs & Metrics tab pivots on `prompt_version_id`
//! so every metric can answer "compared to what?" (doc/logs_metrics_tab.md §4).

mod error;
mod pg_store;
mod store;
mod types;

pub use error::PromptVersionError;
pub use pg_store::PgPromptVersionStore;
#[cfg(test)]
pub use store::InMemoryPromptVersionStore;
pub use store::{PromptVersionStore, SharedPromptVersionStore};
pub use types::{
    NewPromptVersion, PromptVersionId, PromptVersionNumber, PromptVersionRow, RestorePayload,
};
