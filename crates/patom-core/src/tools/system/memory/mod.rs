//! Memory tools (doc/memory.md §1.4, §1.5).
//!
//! Four tools, each in its own file. The three mutation tools share one
//! [`PerTurnCallCounter`] so a single turn cannot exceed
//! [`MAX_MEMORY_MUTATIONS_PER_TURN`] across them combined; `recall` carries
//! its own counter with a separate cap so reads don't spend the mutation
//! budget.
//!
//! - [`MemoryWriteTool`] (`memory_write`) — mints a new memory at
//!   `Tentative`.
//! - [`MemoryUpdateTool`] (`memory_update`) — replaces content; resets
//!   state to `Tentative`. Pinned rows reject agent edits.
//! - [`MemoryForgetTool`] (`memory_forget`) — drops the materialized row;
//!   the journal keeps the event so the operator can revert.
//! - [`MemoryValidateTool`] (`memory_validate`) — records that
//!   independent evidence confirms an existing memory; advances the
//!   validation clock without changing content.
//! - [`RecallTool`] (`recall`) — embedding-driven retrieval against the
//!   agent's existing memories.
//!
//! Shared infrastructure ([`MemoryToolDeps`] + the per-turn counter +
//! handle resolution) lives here in the module root.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::agents::AgentId;
use crate::memory::{
    MAX_MEMORY_MUTATIONS_PER_TURN, MemoryEventId, MemoryHandle, MemoryId, MemorySectionLoader,
    MemoryStoreError, ResolutionOutcome, SharedMemoryStore,
};
use crate::runtime::{PromptRequestId, RequestKindPayload};
use crate::session::SessionId;
use crate::types::ParseError;

use super::super::traits::{ToolCallContext, ToolError};

mod forget;
mod recall;
mod update;
mod validate;
mod write;

pub use forget::MemoryForgetTool;
pub use recall::RecallTool;
pub use update::MemoryUpdateTool;
pub use validate::MemoryValidateTool;
pub use write::MemoryWriteTool;

/// Per-turn call counter shared by the memory tools.
///
/// Bounded HashMap keyed by request id; once a turn hits `cap_per_turn`,
/// further calls return [`CapExceeded`]. Entries are bulk-evicted when the
/// map fills so memory cannot grow without bound across long-running
/// processes — older counters were already enforced live, so clearing does
/// not retroactively let a turn exceed the cap.
///
/// The mutation tools share one counter (combined
/// `write`+`update`+`forget`+`validate` cap — each writes to the journal
/// and is rate-bound by the same per-turn budget); `recall` holds its own
/// with a separate cap so the model can read memory without spending its
/// mutation budget.
#[derive(Debug)]
pub(super) struct PerTurnCallCounter {
    inner: Mutex<HashMap<PromptRequestId, usize>>,
    cap_per_turn: usize,
    bookkeeping_max_entries: usize,
}

impl PerTurnCallCounter {
    fn mutations() -> Self {
        Self::with_cap(MAX_MEMORY_MUTATIONS_PER_TURN)
    }

    pub(super) fn with_cap(cap_per_turn: usize) -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
            cap_per_turn,
            bookkeeping_max_entries: 1024,
        }
    }

    pub(super) fn try_increment(&self, request_id: PromptRequestId) -> Result<usize, CapExceeded> {
        let mut map = self
            .inner
            .lock()
            .expect("invariant: PerTurnCallCounter mutex never poisoned");
        if map.len() >= self.bookkeeping_max_entries && !map.contains_key(&request_id) {
            map.clear();
        }
        let entry = map.entry(request_id).or_insert(0);
        if *entry >= self.cap_per_turn {
            return Err(CapExceeded {
                cap: self.cap_per_turn,
            });
        }
        *entry += 1;
        Ok(*entry)
    }
}

#[derive(Debug)]
pub(super) struct CapExceeded {
    pub cap: usize,
}

/// Shared infrastructure the four memory tools hold a handle on.
///
/// Owns the section loader (mutation tools go through `loader.store()` for
/// storage and through the loader itself for `M-NN` handle resolution) and
/// the per-turn mutation counter.
#[derive(Debug, Clone)]
pub struct MemoryToolDeps {
    pub(super) loader: MemorySectionLoader,
    pub(super) counter: Arc<PerTurnCallCounter>,
}

impl MemoryToolDeps {
    #[must_use]
    pub fn new(loader: MemorySectionLoader) -> Self {
        Self {
            loader,
            counter: Arc::new(PerTurnCallCounter::mutations()),
        }
    }

    /// Direct handle on the underlying memory store — sourced from the
    /// section loader so the renderer and the mutation tools cannot bind
    /// against different stores. `recall`, the mutation tools, and the
    /// resolution-close helper all read through here.
    pub(super) fn store(&self) -> &SharedMemoryStore {
        self.loader.store()
    }
}

pub(super) fn expect_agent(ctx: &ToolCallContext) -> Result<AgentId, ToolError> {
    ctx.viewer
        .agent_id()
        .ok_or_else(|| ToolError::Backend("memory tool invoked with non-agent viewer".into()))
}

pub(super) fn check_cap(
    counter: &PerTurnCallCounter,
    request_id: PromptRequestId,
) -> Result<(), ToolError> {
    counter.try_increment(request_id).map(|_| ()).map_err(|e| {
        ToolError::InvalidInput(format!(
            "memory mutation cap exceeded for this turn (max {} mutations)",
            e.cap
        ))
    })
}

pub(super) fn parse_to_tool_err(e: ParseError) -> ToolError {
    ToolError::InvalidInput(e.to_string())
}

pub(super) fn store_to_tool_err(e: MemoryStoreError) -> ToolError {
    match e {
        MemoryStoreError::NotFound { .. }
        | MemoryStoreError::EventNotFound { .. }
        | MemoryStoreError::ContradictionNotFound(_)
        | MemoryStoreError::WrongAgent { .. }
        | MemoryStoreError::PinnedImmutable { .. }
        | MemoryStoreError::Parse(_) => ToolError::InvalidInput(e.to_string()),
        MemoryStoreError::Db(_) | MemoryStoreError::Provider(_) => {
            ToolError::Backend(e.to_string())
        }
    }
}

/// If the current turn is a librarian-flagged resolution, close the
/// contradiction with the mutation event id as the audit record.
pub(super) async fn maybe_close_resolution(
    deps: &MemoryToolDeps,
    ctx: &ToolCallContext,
    event_id: MemoryEventId,
) -> Result<(), ToolError> {
    if let RequestKindPayload::Resolution {
        contradiction_event_id,
    } = &ctx.kind_payload
    {
        deps.store()
            .resolve_contradiction_for_user(
                ctx.acting_user_id,
                *contradiction_event_id,
                ResolutionOutcome::Mutation(event_id),
            )
            .await
            .map_err(store_to_tool_err)?;
    }
    Ok(())
}

/// Resolve a session-scoped `M-NN` handle to its underlying memory id.
///
/// Delegates to the shared [`MemorySectionLoader`] so the renderer and
/// the mutation tools cannot bind divergent values into the same cache
/// entry. On cache miss the loader composes the section in-line; a
/// session that just rolled past TTL pays one cache reload, not an
/// error.
pub(super) async fn resolve_handle(
    deps: &MemoryToolDeps,
    session: SessionId,
    agent: AgentId,
    kind_payload: &RequestKindPayload,
    handle: MemoryHandle,
) -> Result<MemoryId, ToolError> {
    let resolved = deps
        .loader
        .resolve_handle(session, agent, kind_payload, handle)
        .await
        .map_err(|e| ToolError::Backend(e.to_string()))?;
    resolved.ok_or_else(|| {
        ToolError::InvalidInput(format!(
            "unknown memory handle {handle}; check the `## Memory` section in your system prompt for valid handles"
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counter_caps_at_per_turn_limit() {
        let counter = PerTurnCallCounter::mutations();
        let req = PromptRequestId::new();
        for i in 1..=MAX_MEMORY_MUTATIONS_PER_TURN {
            assert_eq!(counter.try_increment(req).expect("under cap"), i);
        }
        assert!(counter.try_increment(req).is_err());
    }

    #[test]
    fn counter_is_per_request() {
        let counter = PerTurnCallCounter::mutations();
        let r1 = PromptRequestId::new();
        let r2 = PromptRequestId::new();
        for _ in 0..MAX_MEMORY_MUTATIONS_PER_TURN {
            counter.try_increment(r1).expect("r1 ok");
        }
        // r2's quota is fresh.
        counter.try_increment(r2).expect("r2 ok");
    }
}
