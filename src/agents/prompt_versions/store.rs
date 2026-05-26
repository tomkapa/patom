//! Storage trait + cheap-clone handle for the prompt-version history.

use std::fmt;
use std::sync::Arc;

use async_trait::async_trait;

use crate::agents::AgentId;

use super::error::PromptVersionError;
use super::types::{NewPromptVersion, PromptVersionRow};

/// Append-only seam for `agent_prompt_versions`.
#[async_trait]
pub trait PromptVersionStore: fmt::Debug + Send + Sync {
    /// Insert a new version row, computing `version = max(version) + 1`
    /// inside the store transaction. The caller is the HTTP handler that
    /// also UPDATEs `agents` — passing `tx` would couple the two
    /// transactions, so the simple shape here is "store opens its own tx".
    /// In practice the handler invokes both in sequence under the same
    /// privileged path; race-windowed double-bumps are constrained by the
    /// `UNIQUE (agent_id, version)` constraint.
    async fn insert_bump(
        &self,
        payload: NewPromptVersion,
    ) -> Result<PromptVersionRow, PromptVersionError>;

    /// Fetch the current (latest) version for an agent. Used by the
    /// turn recorder to attach `prompt_version_id` to each `turn_metrics`
    /// row. Returns [`PromptVersionError::NoVersionsForAgent`] when the
    /// agent has no seeded v1 — that would indicate the migration didn't
    /// run, which is an operational fault.
    async fn current_for_agent(
        &self,
        agent_id: AgentId,
    ) -> Result<PromptVersionRow, PromptVersionError>;
}

/// Cheap-clone handle so collaborators can hold the store without a generic.
pub type SharedPromptVersionStore = Arc<dyn PromptVersionStore>;

#[cfg(test)]
pub use in_memory::InMemoryPromptVersionStore;

#[cfg(test)]
mod in_memory {
    use std::collections::BTreeMap;
    use std::sync::Mutex;

    use chrono::{DateTime, Utc};

    use crate::agents::AgentId;
    use crate::clock::SharedClock;

    use super::super::error::PromptVersionError;
    use super::super::types::{
        NewPromptVersion, PromptVersionId, PromptVersionNumber, PromptVersionRow,
    };
    use super::PromptVersionStore;

    /// Test double — single-threaded keyed map. Mirrors the in-memory
    /// fake pattern in [`crate::tools::recorder`].
    pub struct InMemoryPromptVersionStore {
        clock: SharedClock,
        rows: Mutex<BTreeMap<AgentId, Vec<PromptVersionRow>>>,
    }

    impl InMemoryPromptVersionStore {
        #[must_use]
        pub fn new(clock: SharedClock) -> Self {
            Self {
                clock,
                rows: Mutex::new(BTreeMap::new()),
            }
        }

        fn now(&self) -> DateTime<Utc> {
            self.clock.now_utc()
        }
    }

    impl std::fmt::Debug for InMemoryPromptVersionStore {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("InMemoryPromptVersionStore")
                .finish_non_exhaustive()
        }
    }

    #[async_trait::async_trait]
    impl PromptVersionStore for InMemoryPromptVersionStore {
        async fn insert_bump(
            &self,
            payload: NewPromptVersion,
        ) -> Result<PromptVersionRow, PromptVersionError> {
            let mut guard = self.rows.lock().expect("invariant: lock not poisoned");
            let list = guard.entry(payload.agent_id).or_default();
            let version = list
                .iter()
                .map(|r| r.version)
                .max()
                .map_or(PromptVersionNumber::FIRST, PromptVersionNumber::next);
            let row = PromptVersionRow {
                id: PromptVersionId::new(),
                agent_id: payload.agent_id,
                org_id: payload.org_id,
                version,
                system_prompt: payload.system_prompt,
                model: payload.model,
                edited_by: payload.edited_by,
                created_at: self.now(),
            };
            list.push(row.clone());
            Ok(row)
        }

        async fn current_for_agent(
            &self,
            agent_id: AgentId,
        ) -> Result<PromptVersionRow, PromptVersionError> {
            let guard = self.rows.lock().expect("invariant: lock not poisoned");
            guard
                .get(&agent_id)
                .and_then(|v| v.iter().max_by_key(|r| r.version).cloned())
                .ok_or(PromptVersionError::NoVersionsForAgent(agent_id))
        }
    }
}
