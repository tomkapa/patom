//! Append-only history of an agent's `system_prompt`.
//!
//! Every `PATCH /agents/:id` that actually changes `system_prompt`
//! inserts a new row in the same transaction as the `agents` UPDATE
//! (see `PgAgentStore::update`'s `bump_prompt_version` helper).
//! Restore is also append-only — every revert mints a fresh
//! `MAX(version) + 1` row byte-identical to the target. The Logs &
//! Metrics tab pivots on `prompt_version_id` so every metric can answer
//! "compared to what?" (doc/logs_metrics_tab.md §4).
//!
//! Model selection is intentionally NOT versioned here — it lives on
//! `agents.model` and is mutated in place. The `turn_metrics` table
//! still snapshots `(prompt_version_id, model)` per call so analytics
//! can pivot on either dimension independently.
//!
//! This module is types-and-errors only. The earlier `PromptVersionStore`
//! trait + Pg/in-memory impls were removed once the version write moved
//! inside `PgAgentStore` — keeping the bump and the agents UPDATE in one
//! transaction is the whole point of the new design.

mod error;
mod types;

pub use error::PromptVersionError;
pub use types::{PromptVersionId, PromptVersionNumber, PromptVersionRow};
