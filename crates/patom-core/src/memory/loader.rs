//! Shared loader for composed memory sections.
//!
//! Two seams need the same loader closure: [`AgentMemory`](super::agent::AgentMemory)
//! (building the `<memory>` system-prompt block) and `MemoryToolDeps`
//! (resolving `M-NN` handles inside mutation tools). Both compose the same
//! stable section, so the `M-NN` handles a tool resolves match the ones the
//! system prompt rendered.
//!
//! [`MemorySectionLoader`] owns every handle the load path needs
//! (`store`, `colleagues`, `roster_cache`, `embeddings`) and exposes one
//! `load_stable(agent)` method. Both seams delegate to it.

use std::sync::Arc;

use crate::agents::AgentId;
use crate::colleagues::{ColleagueId, ColleagueRosterCache, SharedColleagueStore};
use crate::memory::ContradictionEventId;
use crate::provider::SharedEmbeddingProvider;
use crate::runtime::RequestKindPayload;

use super::composer::{MemorySection, SubjectNames, compose_memory_section};
use super::store::{MemoryRow, MemoryStore, SharedMemoryStore};
use super::traits::MemoryError;
use super::types::{MemoryHandle, MemoryId};

/// Cheap-clone bundle of every handle the section-load path needs.
///
/// Every field is `Arc`-backed; clones share the underlying state.
#[derive(Debug, Clone)]
pub struct MemorySectionLoader {
    store: SharedMemoryStore,
    colleagues: SharedColleagueStore,
    roster_cache: ColleagueRosterCache,
    #[allow(dead_code)]
    // retained for the contextual layer rehome onto the thread feed (doc/memory.md §1.3)
    embeddings: SharedEmbeddingProvider,
}

impl MemorySectionLoader {
    #[must_use]
    pub fn new(
        store: SharedMemoryStore,
        colleagues: SharedColleagueStore,
        roster_cache: ColleagueRosterCache,
        embeddings: SharedEmbeddingProvider,
    ) -> Self {
        Self {
            store,
            colleagues,
            roster_cache,
            embeddings,
        }
    }

    /// Direct access to the underlying memory store. Mutation tools
    /// (`memory_write` / `memory_update` / `memory_forget`) and `recall`
    /// call `store.apply` / `store.search_by_embedding` directly; routing
    /// every store call through the loader would add a level of
    /// indirection for no benefit.
    #[must_use]
    pub fn store(&self) -> &SharedMemoryStore {
        &self.store
    }

    /// Compose the agent's **stable** memory section only — pinned + Identity
    /// rows, trimmed to byte budget — with no session-keyed contextual layer.
    ///
    /// The thread-feed chat path ([`Memory::system_prompt_for_thread`]) has no
    /// `SessionId` to key the opening-message retrieval on, so the contextual
    /// layer is omitted (it degrades empty in the session path too; it is
    /// enrichment, never load-bearing — doc/memory.md §1.3). Computed fresh on
    /// every call: there is no thread-keyed cache yet, and the stable layer is a
    /// single `store.list(agent)` read.
    ///
    /// [`Memory::system_prompt`]: super::Memory::system_prompt
    /// [`Memory::system_prompt_for_thread`]: super::Memory::system_prompt_for_thread
    pub async fn load_stable(
        &self,
        agent: AgentId,
        kind_payload: &RequestKindPayload,
    ) -> Result<Arc<MemorySection>, MemoryError> {
        let contradiction = match kind_payload {
            RequestKindPayload::Resolution {
                contradiction_event_id,
            } => Some(*contradiction_event_id),
            _ => None,
        };
        let rows = self
            .store
            .list(agent)
            .await
            .map_err(|e| MemoryError::Backend(e.to_string()))?;
        let reserved = resolve_reserved_pair(&*self.store, contradiction).await?;
        let subjects = hydrate_subjects(&self.roster_cache, &self.colleagues, rows.iter()).await?;
        // No contextual layer in the thread path (no session-keyed opening
        // message to retrieve against) — an empty slice, not a wasted alloc.
        Ok(Arc::new(compose_memory_section(
            &rows,
            &[],
            &reserved,
            &subjects,
        )))
    }

    /// Resolve a `M-NN` handle to its underlying memory id. Returns `None`
    /// when the handle was never minted for this agent's stable section (a
    /// hallucinated reference, or a row that has since been forgotten).
    ///
    /// Composes the stable section fresh — the same path the system prompt
    /// took this turn — so the handles a tool resolves match what the model
    /// saw rendered.
    pub async fn resolve_handle(
        &self,
        agent: AgentId,
        kind_payload: &RequestKindPayload,
        handle: MemoryHandle,
    ) -> Result<Option<MemoryId>, MemoryError> {
        let section = self.load_stable(agent, kind_payload).await?;
        Ok(section.resolve_handle(handle))
    }
}

/// Build the [`SubjectNames`] map for every `Collaborator` memory among
/// `rows` by reading the org roster once. Returns an empty map (no roster
/// read) when no loaded row names a subject — the common case for agents that
/// hold no collaborator memories. A subject that the roster does not contain
/// (a coworker who has since left the org) is simply absent from the map; the
/// renderer then degrades that entry to plain prose.
///
/// The roster comes through the shared [`ColleagueRosterCache`] — the same
/// cache the system-prompt `<colleagues>` block reads — so a hydration on an
/// org already rendered this turn is a cache hit, not a second DB scan.
async fn hydrate_subjects<'a>(
    roster_cache: &ColleagueRosterCache,
    colleagues: &SharedColleagueStore,
    rows: impl Iterator<Item = &'a MemoryRow>,
) -> Result<SubjectNames, MemoryError> {
    let mut needed: std::collections::HashSet<ColleagueId> = std::collections::HashSet::new();
    let mut org = None;
    for r in rows {
        if let Some(subject) = r.subject {
            needed.insert(subject);
            org = Some(r.org_id);
        }
    }
    let Some(org) = org else {
        return Ok(SubjectNames::new());
    };

    let roster = roster_cache.get_or_load(org, colleagues).await?;
    let mut out = SubjectNames::with_capacity(needed.len());
    for cref in roster.iter() {
        if needed.contains(&cref.id) {
            out.insert(cref.id, cref.display_name.clone());
        }
    }
    Ok(out)
}

/// Read the contradiction row and return `[memory_a, memory_b]` in
/// column order — the composer's reserved-handle binding. `None` for the
/// id, or a row that has gone missing between detection and the
/// resolution turn, both yield an empty vector; the composer then renders
/// the layered section without reservations and the model degrades to a
/// no-action close.
async fn resolve_reserved_pair(
    store: &dyn MemoryStore,
    contradiction: Option<ContradictionEventId>,
) -> Result<Vec<MemoryId>, MemoryError> {
    let Some(id) = contradiction else {
        return Ok(Vec::new());
    };
    let row = store
        .read_contradiction(id)
        .await
        .map_err(|e| MemoryError::Backend(e.to_string()))?;
    Ok(row.map_or_else(Vec::new, |r| vec![r.memory_a, r.memory_b]))
}
