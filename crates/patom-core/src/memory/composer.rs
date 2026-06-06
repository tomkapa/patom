//! Composes the rendered memory section of the system prompt
//! (doc/memory.md §1.3).
//!
//! Two layers come out of one call:
//!
//! - *Stable* — pinned + Self-kind, sorted by state then recency, trimmed
//!   to a byte budget.
//! - *Contextual* — top-K Other / Procedure / Open already ranked by the
//!   loader (cosine similarity against the session's opening text).
//!
//! The output carries both the rendered text and a per-section
//! [`MemoryHandleMap`] (`M-NN ↔ MemoryId`) so the agent's tool calls
//! can be resolved back to UUIDs.

use std::collections::HashMap;
use std::sync::Arc;

use super::limits::{
    CONTEXTUAL_LAYER_MAX_BYTES, RENDER_LINE_OVERHEAD_BYTES, STABLE_LAYER_MAX_BYTES,
};
use super::store::MemoryRow;
use super::types::{MemoryHandle, MemoryId, MemoryKind};

/// Stable XML-ish tags wrapping the rendered memory section. Matches the
/// existing `<core>...</core>` / `<role>...</role>` framing so the model
/// sees one consistent envelope per system block.
pub const MEMORY_TAG_OPEN: &str = "<memory>\n";
pub const MEMORY_TAG_CLOSE: &str = "\n</memory>";

/// Order section headers in the rendered output. Pinned-or-Self memories
/// can land in any kind, but kinds appear in this fixed sequence so
/// section structure stays stable across compositions.
const KIND_ORDER: &[MemoryKind] = &[
    MemoryKind::Identity,
    MemoryKind::Other,
    MemoryKind::Collaborator,
    MemoryKind::Procedure,
    MemoryKind::Open,
];

/// Rendered memory section + the (handle ↔ id) map the rendering
/// produced.
///
/// `text` includes the surrounding `<memory>...</memory>` envelope when
/// non-empty; empty memory yields an empty `text` so callers can append
/// it unconditionally. The handle map is private — callers reach
/// [`Self::resolve_handle`] for the only operation they need.
#[derive(Debug, Clone)]
pub struct MemorySection {
    text: Arc<str>,
    handles: HashMap<MemoryHandle, MemoryId>,
}

impl MemorySection {
    #[must_use]
    pub fn empty() -> Self {
        Self {
            text: Arc::from(""),
            handles: HashMap::new(),
        }
    }

    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }

    /// Resolve a session-scoped `M-NN` handle back to its `MemoryId`.
    /// Returns `None` if the handle was never minted for this section
    /// (a hallucinated reference, or a section whose cache entry has
    /// been evicted and recomposed without the row).
    #[must_use]
    pub fn resolve_handle(&self, handle: MemoryHandle) -> Option<MemoryId> {
        self.handles.get(&handle).copied()
    }

    /// `text` is empty when no row contributed a rendered entry. The
    /// handle map may still hold reserved bindings (e.g. the `M-1`/`M-2`
    /// pair for a resolution turn) without producing visible text.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }
}

/// Compose the memory section from the agent's full row set and an
/// optional pre-retrieved contextual layer.
///
/// `rows` is the entire materialized table for the agent (capped per
/// agent at [`super::limits::MAX_MEMORIES_PER_AGENT`]); the composer
/// filters into the stable layer. `contextual` is the top-K rows the
/// embedding search returned, already in score order; pass `&[]` when
/// no embedding query is available (e.g. operator audit endpoints).
/// The composer trims contextual rows to the contextual byte budget
/// and skips any whose id already appears in the stable layer.
///
/// `reserved` claims `M-1..=M-N` for the given memory ids regardless of
/// whether those rows appear in the layered selections. Layered rows
/// whose id is in `reserved` are deduped from the rendered text (no
/// double-render); their entries are kept only at their reserved handle.
/// Remaining layered rows mint `M-(N+1)..`. Pass `&[]` for the common
/// kind-agnostic path; the resolution turn passes `[memory_a, memory_b]`
/// so the librarian-flagged pair binds `M-1` / `M-2`.
#[must_use]
pub fn compose_memory_section<'a>(
    rows: &'a [MemoryRow],
    contextual: &'a [&'a MemoryRow],
    reserved: &[MemoryId],
) -> MemorySection {
    let stable = select_stable_layer(rows);
    let stable_ids: std::collections::HashSet<MemoryId> = stable.iter().map(|r| r.id).collect();
    let trimmed_contextual: Vec<&MemoryRow> = trim_to_budget(
        contextual
            .iter()
            .copied()
            .filter(|r| !stable_ids.contains(&r.id))
            .collect(),
        super::limits::CONTEXTUAL_LAYER_MAX_BYTES,
    );
    render(&stable, &trimmed_contextual, reserved)
}

/// Stable layer per doc/memory.md §1.3: pinned + Self-kind, sorted by
/// state (`Core > Validated > Held > Tentative`) then by created_at
/// descending. Trimmed to [`STABLE_LAYER_MAX_BYTES`].
fn select_stable_layer(rows: &[MemoryRow]) -> Vec<&MemoryRow> {
    let mut candidates: Vec<&MemoryRow> = rows
        .iter()
        .filter(|r| r.pinned || r.kind == MemoryKind::Identity)
        .collect();
    candidates.sort_by(|a, b| {
        b.state
            .priority()
            .cmp(&a.state.priority())
            .then_with(|| b.created_at.cmp(&a.created_at))
    });
    trim_to_budget(candidates, STABLE_LAYER_MAX_BYTES)
}

fn trim_to_budget(rows: Vec<&MemoryRow>, budget: usize) -> Vec<&MemoryRow> {
    let mut total = 0usize;
    let mut kept = Vec::with_capacity(rows.len());
    for row in rows {
        let line = row.content.len() + RENDER_LINE_OVERHEAD_BYTES;
        if total.saturating_add(line) > budget {
            break;
        }
        total += line;
        kept.push(row);
    }
    kept
}

fn render(
    stable: &[&MemoryRow],
    contextual: &[&MemoryRow],
    reserved: &[MemoryId],
) -> MemorySection {
    // Reserved handles bind even when no layered row is rendered, so
    // tool-call resolution still succeeds for callers that pre-allocated
    // them.
    let (mut handles, mut next_handle) = bind_reserved_handles(reserved);
    let reserved_ids: std::collections::HashSet<MemoryId> = reserved.iter().copied().collect();

    let mut by_kind: HashMap<MemoryKind, Vec<(MemoryHandle, &MemoryRow)>> = HashMap::new();
    for row in stable.iter().chain(contextual.iter()) {
        // A row whose id is already bound at a reserved handle is
        // suppressed from the rendered text — the caller surfaces it
        // elsewhere; rendering it again would double-name the same id.
        if reserved_ids.contains(&row.id) {
            continue;
        }
        let handle = MemoryHandle::try_from(next_handle)
            .expect("invariant: handle count under cap (per-agent quota enforces it)");
        next_handle = next_handle
            .checked_add(1)
            .expect("invariant: handle counter cannot overflow under per-agent quota");
        handles.insert(handle, row.id);
        by_kind.entry(row.kind).or_default().push((handle, *row));
    }

    if by_kind.is_empty() {
        return MemorySection {
            text: Arc::from(""),
            handles,
        };
    }

    MemorySection {
        text: Arc::from(render_text(&by_kind)),
        handles,
    }
}

/// Mint a `M-NN` handle for every reserved memory id in order and return
/// the bound map plus the next free handle counter.
fn bind_reserved_handles(reserved: &[MemoryId]) -> (HashMap<MemoryHandle, MemoryId>, u32) {
    let mut handles: HashMap<MemoryHandle, MemoryId> = HashMap::new();
    let mut next_handle: u32 = 1;
    for id in reserved {
        let handle = MemoryHandle::try_from(next_handle)
            .expect("invariant: reserved handle count under cap");
        next_handle = next_handle
            .checked_add(1)
            .expect("invariant: handle counter cannot overflow");
        handles.insert(handle, *id);
    }
    (handles, next_handle)
}

fn render_text(by_kind: &HashMap<MemoryKind, Vec<(MemoryHandle, &MemoryRow)>>) -> String {
    let estimated = STABLE_LAYER_MAX_BYTES + CONTEXTUAL_LAYER_MAX_BYTES + 256;
    let mut buf = String::with_capacity(estimated);
    buf.push_str(MEMORY_TAG_OPEN);
    buf.push_str("## Memory\n");
    buf.push_str(
        "Facts you previously remembered. Each entry has an `M-NN` handle — \
        pass it to `memory_update`, `memory_forget`, or `memory_validate` to \
        revise the entry. Use `recall` to search for memories not listed \
        below.\n",
    );

    for kind in KIND_ORDER {
        let Some(entries) = by_kind.get(kind) else {
            continue;
        };
        if entries.is_empty() {
            continue;
        }
        buf.push('\n');
        buf.push_str("### ");
        buf.push_str(kind.display_label());
        buf.push('\n');
        for (handle, row) in entries {
            // `- [<handle>, <state>] <content>` per doc/memory.md §1.3.
            buf.push_str("- [");
            buf.push_str(&handle.to_string());
            buf.push_str(", ");
            buf.push_str(row.state.as_str());
            buf.push_str("] ");
            buf.push_str(row.content.as_str());
            buf.push('\n');
        }
    }

    buf.push_str(MEMORY_TAG_CLOSE);
    buf
}

#[cfg(test)]
mod tests {
    use chrono::{TimeZone, Utc};

    use crate::agents::AgentId;
    use crate::memory::store::MemoryRow;
    use crate::memory::types::{MemoryContent, MemoryKind, MemoryState};

    use super::*;

    fn row(
        ix: u8,
        kind: MemoryKind,
        content: &str,
        state: MemoryState,
        pinned: bool,
        ts_secs: i64,
    ) -> MemoryRow {
        MemoryRow {
            id: MemoryId::new(),
            agent_id: AgentId::new(),
            org_id: crate::auth::OrgId::new(),
            kind,
            content: MemoryContent::try_from(content).expect("valid"),
            state,
            pinned,
            source_turn_id: None,
            created_at: Utc.timestamp_opt(ts_secs, 0).unwrap(),
            last_validated_at: Utc.timestamp_opt(ts_secs, 0).unwrap(),
            last_accessed_at: Utc.timestamp_opt(ts_secs, 0).unwrap(),
            access_count: u64::from(ix),
        }
    }

    #[test]
    fn empty_input_renders_empty_section() {
        let section = compose_memory_section(&[], &[], &[]);
        assert!(section.is_empty());
        assert!(section.is_empty());
    }

    #[test]
    fn stable_layer_filters_to_pinned_or_self() {
        let rows = vec![
            row(
                1,
                MemoryKind::Identity,
                "self memory",
                MemoryState::Held,
                false,
                100,
            ),
            row(
                2,
                MemoryKind::Other,
                "other memory",
                MemoryState::Held,
                false,
                200,
            ),
            row(
                3,
                MemoryKind::Procedure,
                "pinned proc",
                MemoryState::Core,
                true,
                300,
            ),
        ];
        let section = compose_memory_section(&rows, &[], &[]);
        let txt = section.text();
        assert!(txt.contains("self memory"), "Self kind included: {txt}");
        assert!(
            txt.contains("pinned proc"),
            "pinned non-Self included: {txt}"
        );
        assert!(
            !txt.contains("other memory"),
            "non-pinned non-Self excluded: {txt}"
        );
    }

    #[test]
    fn stable_layer_sorted_by_state_then_recency() {
        let rows = vec![
            row(
                1,
                MemoryKind::Identity,
                "tentative-old",
                MemoryState::Tentative,
                false,
                100,
            ),
            row(
                2,
                MemoryKind::Identity,
                "validated-old",
                MemoryState::Validated,
                false,
                100,
            ),
            row(
                3,
                MemoryKind::Identity,
                "held-new",
                MemoryState::Held,
                false,
                999,
            ),
            row(
                4,
                MemoryKind::Identity,
                "validated-new",
                MemoryState::Validated,
                false,
                500,
            ),
        ];
        let section = compose_memory_section(&rows, &[], &[]);
        let txt = section.text();
        let pos = |s: &str| txt.find(s).expect("present");
        // Validated > Held > Tentative; within Validated the newer one first.
        assert!(pos("validated-new") < pos("validated-old"));
        assert!(pos("validated-old") < pos("held-new"));
        assert!(pos("held-new") < pos("tentative-old"));
    }

    #[test]
    fn handle_map_round_trips_to_memory_id() {
        let rows = vec![row(
            1,
            MemoryKind::Identity,
            "first",
            MemoryState::Held,
            false,
            100,
        )];
        let section = compose_memory_section(&rows, &[], &[]);
        let id = rows[0].id;
        let handle = MemoryHandle::try_from(1u32).expect("valid");
        assert_eq!(section.resolve_handle(handle), Some(id));
        assert!(section.text().contains("M-1"));
    }

    #[test]
    fn render_uses_state_label_and_handle() {
        let rows = vec![row(
            1,
            MemoryKind::Identity,
            "terse replies",
            MemoryState::Validated,
            false,
            100,
        )];
        let section = compose_memory_section(&rows, &[], &[]);
        assert!(
            section.text().contains("- [M-1, validated] terse replies"),
            "render shape: {}",
            section.text()
        );
        assert!(section.text().contains("### Self"));
        assert!(
            section.text().starts_with(MEMORY_TAG_OPEN),
            "wrapped in <memory>: {}",
            section.text()
        );
        assert!(section.text().ends_with(MEMORY_TAG_CLOSE));
    }

    #[test]
    fn trim_drops_overflow_past_budget() {
        // Each row is ~CONTENT bytes; with overhead, fit only a handful
        // before tripping the cap. Use a small content size and many rows
        // to force the trim.
        let big_content = "x".repeat(STABLE_LAYER_MAX_BYTES / 4);
        let rows: Vec<MemoryRow> = (0..10)
            .map(|i| {
                row(
                    i,
                    MemoryKind::Identity,
                    big_content.as_str(),
                    MemoryState::Held,
                    false,
                    i64::from(i),
                )
            })
            .collect();
        let section = compose_memory_section(&rows, &[], &[]);
        // Must include at least one row, but fewer than all 10.
        let count = section.text().matches("- [M-").count();
        assert!(count >= 1, "at least one row kept");
        assert!(count < rows.len(), "trimmed below row count");
    }

    #[test]
    fn kinds_render_in_fixed_order() {
        // Two pinned memories, one Other (later by recency), one
        // Procedure (earlier). Render order must be Other → Procedure
        // regardless of recency, since kind ordering is fixed.
        let rows = vec![
            row(
                1,
                MemoryKind::Procedure,
                "pin-proc",
                MemoryState::Core,
                true,
                100,
            ),
            row(
                2,
                MemoryKind::Other,
                "pin-other",
                MemoryState::Core,
                true,
                200,
            ),
        ];
        let section = compose_memory_section(&rows, &[], &[]);
        let txt = section.text();
        let other = txt.find("### Other").expect("### Other present");
        let proc = txt.find("### Procedure").expect("### Procedure present");
        assert!(other < proc, "Other section precedes Procedure: {txt}");
    }

    #[test]
    fn reserved_handles_bind_without_rendering_pair() {
        // The pair side is delivered to the model via the user prompt
        // body; the system-prompt `<memory>` block must still resolve
        // `M-1` / `M-2` to the right ids but must not render them.
        let pair_a = MemoryId::new();
        let pair_b = MemoryId::new();
        let section = compose_memory_section(&[], &[], &[pair_a, pair_b]);
        assert_eq!(section.text(), "", "no text when only reserved handles");
        assert!(section.is_empty());
        let h1 = MemoryHandle::try_from(1u32).expect("M-1");
        let h2 = MemoryHandle::try_from(2u32).expect("M-2");
        assert_eq!(section.resolve_handle(h1), Some(pair_a));
        assert_eq!(section.resolve_handle(h2), Some(pair_b));
    }

    #[test]
    fn reserved_pair_dedups_from_layered_text_and_offsets_handles() {
        // Pair-side row that also qualifies for the stable layer must
        // appear only once (under its reserved handle), and the rest of
        // the stable layer must mint `M-3..` not `M-1..`.
        let pair_a = row(
            1,
            MemoryKind::Identity,
            "pair-a",
            MemoryState::Held,
            false,
            100,
        );
        let pair_a_id = pair_a.id;
        let pair_b = MemoryId::new();
        let other = row(
            2,
            MemoryKind::Identity,
            "other-self",
            MemoryState::Held,
            false,
            200,
        );
        let other_id = other.id;
        let rows = vec![pair_a, other];

        let section = compose_memory_section(&rows, &[], &[pair_a_id, pair_b]);
        let txt = section.text();
        assert!(!txt.contains("pair-a"), "pair-side row not rendered: {txt}");
        assert!(txt.contains("other-self"), "non-pair row rendered: {txt}");
        assert!(
            txt.contains("- [M-3, held] other-self"),
            "layered row offset to M-3: {txt}"
        );
        let h1 = MemoryHandle::try_from(1u32).expect("M-1");
        let h2 = MemoryHandle::try_from(2u32).expect("M-2");
        let h3 = MemoryHandle::try_from(3u32).expect("M-3");
        assert_eq!(section.resolve_handle(h1), Some(pair_a_id));
        assert_eq!(section.resolve_handle(h2), Some(pair_b));
        assert_eq!(section.resolve_handle(h3), Some(other_id));
    }
}
