//! Shared loader for composed memory sections.
//!
//! Two seams need the same loader closure: [`AgentMemory`](super::agent::AgentMemory)
//! (building the `<memory>` system-prompt block) and `MemoryToolDeps`
//! (resolving `M-NN` handles inside mutation tools). Both write into the
//! same [`SessionMemoryCache`] keyed on `(session, agent)`, so the cached
//! value must be identical regardless of which path performed the load —
//! whichever caller misses the cache first wins, and the other gets a
//! cache hit. A divergent loader (e.g. one builds the contextual layer,
//! the other doesn't) would silently bind the cached value to call order.
//!
//! [`MemorySectionLoader`] owns every handle the load path needs
//! (`store`, `sessions`, `embeddings`, `cache`) and exposes one
//! `load(session, agent)` method. Both seams delegate to it.

use std::sync::Arc;

use tracing::warn;

use crate::agents::AgentId;
use crate::colleagues::ColleagueId;
use crate::memory::ContradictionEventId;
use crate::provider::{
    ChatMessage, EmbeddingProvider, SharedEmbeddingProvider, UserContent, embed_one,
};
use crate::runtime::RequestKindPayload;
use crate::session::{SessionId, SharedSessionStore};

use super::composer::{MemorySection, compose_memory_section};
use super::limits::CONTEXTUAL_TOP_K;
use super::session_cache::SessionMemoryCache;
use super::store::{MemoryRow, MemoryStore, SearchFilter, SharedMemoryStore};
use super::traits::MemoryError;
use super::types::{MemoryHandle, MemoryId, MemoryKind};

/// Cheap-clone bundle of every handle the section-load path needs.
///
/// All four fields are `Arc`-backed; clones share the underlying state.
#[derive(Debug, Clone)]
pub struct MemorySectionLoader {
    store: SharedMemoryStore,
    sessions: SharedSessionStore,
    embeddings: SharedEmbeddingProvider,
    cache: SessionMemoryCache,
}

impl MemorySectionLoader {
    #[must_use]
    pub fn new(
        store: SharedMemoryStore,
        sessions: SharedSessionStore,
        embeddings: SharedEmbeddingProvider,
        cache: SessionMemoryCache,
    ) -> Self {
        Self {
            store,
            sessions,
            embeddings,
            cache,
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

    /// Snapshot of the underlying session cache. Exposed so callers that
    /// need to invalidate or inspect entries can do so without holding a
    /// second handle.
    #[must_use]
    pub fn cache(&self) -> &SessionMemoryCache {
        &self.cache
    }

    /// Load — or compose, on cache miss — the agent's composed memory
    /// section for `(session, agent)`. The loader assembles both layers:
    ///
    /// - **Stable** — pinned + Identity rows, trimmed to byte budget
    ///   (see [`compose_memory_section`]).
    /// - **Contextual** — top-K Other/Procedure/Open rows ranked by
    ///   cosine similarity against the session's opening user message
    ///   (doc/memory.md §1.3).
    ///
    /// The contextual layer is degraded-empty on any of: missing
    /// opening message, snapshot failure, embedding failure, search
    /// failure. The stable layer renders regardless. Errors short-circuit
    /// only when the store's `list` call itself fails — that's a backend
    /// outage that the caller has to surface.
    ///
    /// `kind_payload` selects per-kind composition. For
    /// `RequestKindPayload::Resolution { contradiction_event_id }` the
    /// loader reserves `M-1` / `M-2` for the flagged pair (delivered to
    /// the model via the user prompt body) and the layered selections
    /// mint `M-3..` with the pair-side rows deduped from the rendered
    /// text. Every other variant goes through the default composer.
    pub async fn load(
        &self,
        session: SessionId,
        agent: AgentId,
        kind_payload: &RequestKindPayload,
    ) -> Result<Arc<MemorySection>, MemoryError> {
        let store = self.store.clone();
        let sessions = self.sessions.clone();
        let embeddings = self.embeddings.clone();
        // Cheap variant probe — the DB lookup happens inside the closure
        // so cache hits skip it entirely.
        let contradiction = match kind_payload {
            RequestKindPayload::Resolution {
                contradiction_event_id,
            } => Some(*contradiction_event_id),
            _ => None,
        };
        self.cache
            .get_or_load(session, agent, || async move {
                let rows = store
                    .list(agent)
                    .await
                    .map_err(|e| MemoryError::Backend(e.to_string()))?;
                let reserved = resolve_reserved_pair(&*store, contradiction).await?;

                // Resolve the agent's colleague_id via the session row so
                // `snapshot`'s self-detection matches the right end. Cheaper
                // than threading a `ColleagueStore` through the loader.
                let viewer_colleague =
                    agent_colleague_in_session(&sessions, session, agent).await?;
                let opening = match sessions.snapshot(session, viewer_colleague).await {
                    Ok(snap) => first_user_text(&snap),
                    Err(e) => {
                        warn!(
                            error = %e,
                            patom.session.id = %session,
                            patom.agent.id = %agent,
                            "memory.contextual.snapshot.error"
                        );
                        None
                    }
                };

                let contextual_rows = match opening {
                    Some(text) if !text.is_empty() => {
                        retrieve_contextual(&*store, &*embeddings, agent, &text).await
                    }
                    _ => Vec::new(),
                };
                let contextual_refs: Vec<&MemoryRow> = contextual_rows.iter().collect();

                Ok::<_, MemoryError>(compose_memory_section(&rows, &contextual_refs, &reserved))
            })
            .await
    }

    /// Resolve a session-scoped `M-NN` handle to its underlying memory
    /// id. Returns `None` when the handle was never minted for this
    /// session (a hallucinated reference, or a section whose cache entry
    /// has been evicted and recomposed without the row).
    pub async fn resolve_handle(
        &self,
        session: SessionId,
        agent: AgentId,
        kind_payload: &RequestKindPayload,
        handle: MemoryHandle,
    ) -> Result<Option<MemoryId>, MemoryError> {
        let section = self.load(session, agent, kind_payload).await?;
        Ok(section.resolve_handle(handle))
    }
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

/// First user-role message in `messages`, concatenated `Text` blocks
/// joined by a newline. Returns `None` if no user message exists or it
/// carries no text content. The agent's session always opens with a user
/// message (the human's first prompt or another agent's `send_message`
/// body), so the `None` branch is degraded-only — empty contextual
/// layer.
fn first_user_text(messages: &[ChatMessage]) -> Option<String> {
    for m in messages {
        let ChatMessage::User(blocks) = m else {
            continue;
        };
        let mut out = String::new();
        for b in blocks {
            if let UserContent::Text(s) = b {
                if !out.is_empty() {
                    out.push('\n');
                }
                out.push_str(s);
            }
        }
        if !out.is_empty() {
            return Some(out);
        }
    }
    None
}

/// Embed the opening text and run a top-K similarity search restricted
/// to the contextual kinds (Other / Procedure / Open). Self + pinned
/// rows are already handled by the stable layer, so excluding Identity
/// avoids double-rendering.
///
/// Returns an empty vector on any failure — the stable layer is enough
/// to ship a turn, and a transient embedding outage must not block the
/// system prompt (doc/memory.md §2.9).
async fn retrieve_contextual(
    store: &dyn MemoryStore,
    embeddings: &dyn EmbeddingProvider,
    agent: AgentId,
    text: &str,
) -> Vec<MemoryRow> {
    let query = match embed_one(embeddings, text).await {
        Ok(v) => v,
        Err(e) => {
            warn!(error = %e, patom.agent.id = %agent, "memory.contextual.embed.error");
            return Vec::new();
        }
    };
    let filter = SearchFilter {
        kinds: Some(vec![
            MemoryKind::Other,
            // Collaborator joins the contextual allowlist
            // (doc/agent_discovery_plan.md §4.4) — collaborator knowledge
            // is by nature contextual, surfaced only when the session is
            // about something that calls for delegation.
            MemoryKind::Collaborator,
            MemoryKind::Procedure,
            MemoryKind::Open,
        ]),
        min_state: None,
    };
    match store
        .search_by_embedding(agent, &query, CONTEXTUAL_TOP_K, filter)
        .await
    {
        Ok(scored) => scored.into_iter().map(|s| s.row).collect(),
        Err(e) => {
            warn!(error = %e, patom.agent.id = %agent, "memory.contextual.search.error");
            Vec::new()
        }
    }
}

/// Resolve `agent`'s `ColleagueId` within `session` by inspecting the session
/// participants and finding the side whose `agent_id` matches. The session
/// row already stores the colleague_id; this avoids threading a separate
/// `ColleagueStore` through the loader/AgentMemory plumbing for one lookup.
async fn agent_colleague_in_session(
    sessions: &SharedSessionStore,
    session: SessionId,
    agent: AgentId,
) -> Result<ColleagueId, MemoryError> {
    let (a, b) = sessions
        .participants(session)
        .await
        .map_err(|e| MemoryError::Backend(format!("session participants: {e}")))?;
    let candidate = if a.agent_id() == Some(agent) {
        Some(a)
    } else if b.agent_id() == Some(agent) {
        Some(b)
    } else {
        None
    };
    let participant = candidate.ok_or_else(|| {
        MemoryError::Backend(format!(
            "agent {agent:?} is not a participant of session {session:?}"
        ))
    })?;
    participant.colleague_id().ok_or_else(|| {
        MemoryError::Backend(format!(
            "agent participant in session {session:?} has no colleague_id"
        ))
    })
}
