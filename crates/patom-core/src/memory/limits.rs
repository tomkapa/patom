//! Memory subsystem caps. CLAUDE.md §5 — every value is documented with
//! *why this number*. Caps that gate a specific subsystem (recall,
//! librarian, reflection scheduler) live here so the renderer, store, and
//! background workers all share one source of truth.

/// Maximum bytes of a single memory's `content`.
///
/// One or two sentences (doc/memory.md §1.2) — sized so a stable layer of
/// dozens of memories still fits comfortably under the model's prompt
/// budget. Mirrors the column `CHECK (octet_length(content) BETWEEN 1 AND
/// 4096)` in migration 7.
pub const MEMORY_CONTENT_MAX_BYTES: usize = 4096;

/// Maximum bytes of a `contradiction_events.reason` blurb.
///
/// Librarian-written, never agent-facing — sized so the heuristic that
/// flagged the pair (similarity score, opposing keywords) can be recorded
/// in human-readable form without bloating the table.
pub const CONTRADICTION_REASON_MAX_BYTES: usize = 1024;

/// Maximum bytes of the free-text `evidence` accompanying a validation
/// event (`validation_events.detail`).
///
/// Sized to match the column's `octet_length(detail) BETWEEN 1 AND 1024`
/// CHECK in migration 9. The agent supplies a short citation
/// (web-search snippet, peer-agent quote, human reply) when validating;
/// the operator audit reads this verbatim when assessing whether the
/// validation was real fact-checking or rubber-stamping.
pub const VALIDATION_EVIDENCE_MAX_BYTES: usize = 1024;

/// Hard cap on the number of materialized memory rows held per agent.
///
/// Keeps the renderer's stable + contextual layer bounded and gives the
/// librarian a target for eviction. Read paths assert the cap as a
/// saturation signal — overshooting means the writer ignored the same
/// limit.
pub const MAX_MEMORIES_PER_AGENT: usize = 1024;

/// Hard cap on a single page of journal events returned to in-process
/// callers. The journal grows unbounded with mutation history; the
/// operator audit endpoint paginates through this cap with a cursor.
pub const MAX_EVENTS_PER_PAGE: usize = 1024;

/// Byte budget for the rendered stable layer (pinned + Self) inside the
/// system prompt.
///
/// Sized so a few dozen one-or-two-sentence memories fit without crowding
/// the model's context window. Matches the order of magnitude of an
/// agent's role prompt, so doubling memory does not overwhelm role.
pub const STABLE_LAYER_MAX_BYTES: usize = 4096;

/// Byte budget for the rendered contextual layer (Other / Procedure /
/// Open).
///
/// The contextual layer is per-session, retrieved against the opener;
/// sized smaller than the stable layer because retrieval already narrows
/// the set.
pub const CONTEXTUAL_LAYER_MAX_BYTES: usize = 4096;

/// Top-K cap on the contextual retrieval (cosine similarity).
pub const CONTEXTUAL_TOP_K: usize = 16;

/// Per-line overhead added to a memory's content when estimating render
/// size for the byte budget — covers `- [M-NN, validated] ` plus the
/// trailing newline. A worst-case estimate (handle up to four digits,
/// state label up to nine chars) so trimming never overshoots.
pub(super) const RENDER_LINE_OVERHEAD_BYTES: usize = 32;

/// In-process bound on the per-session memory composition cache.
///
/// Same rationale as `MAX_THREAD_SLOTS` in `runtime::limits`: caps memory
/// growth across a long-lived process. Eviction prefers the oldest
/// expired entry, falling back to the absolute oldest.
pub const SESSION_MEMORY_CACHE_CAP: usize = 1024;

/// TTL on the per-session memory composition cache.
///
/// Long enough that within an active session burst the assembled prompt
/// stays cached (frozen-for-session, doc/memory.md §1.3). Short enough
/// that a session resumed after a long idle picks up new memories
/// written in between.
pub const SESSION_MEMORY_CACHE_TTL_SECS: u64 = 60 * 60;

/// Per-turn cap on memory mutations (write + update + forget combined).
///
/// CLAUDE.md §5: every batch capped. A single normal turn that produces
/// more than this many memory mutations is almost certainly a model
/// runaway — we reject the overflowing call and let the model continue.
/// Reflection turns share the same per-turn cap.
pub const MAX_MEMORY_MUTATIONS_PER_TURN: usize = 8;

/// Per-reflection cap on total mutations.
///
/// doc/memory.md §1.6 — "dreams cannot produce 30 writes a night and
/// bloat the journal with noise". Higher than the per-turn cap because
/// reflection looks at a longer span of conversation, but still bounded.
pub const MAX_MEMORY_MUTATIONS_PER_REFLECTION: usize = 16;

/// Idle timeout after which a session qualifies for a reflection sweep
/// (doc/memory.md §1.6 — "the time since the last turn exceeds a
/// configurable idle timeout").
///
/// Long enough that an active conversation does not get reflected mid-
/// burst; short enough that overnight idleness produces consolidation
/// before the user resumes.
pub const REFLECTION_IDLE_TIMEOUT_SECS: u64 = 30 * 60;

/// Polling cadence for the reflection scheduler. Bounded so a tight
/// loop cannot pin the database under quiescence.
pub const REFLECTION_SCHEDULER_POLL_SECS: u64 = 60;

/// Cap on how many `(agent, session)` pairs the reflection scheduler
/// enqueues per poll. Bounded so a sudden burst of idle sessions cannot
/// overwhelm the queue.
pub const REFLECTION_SCHEDULER_BATCH_LIMIT: usize = 32;

/// Top-K cap applied to a single `recall` tool result. Bounds the worst
/// case bytes the model can pull into context per turn.
pub const RECALL_MAX_RESULTS: u8 = 8;

/// Default top-K for `recall` when the caller omits `limit`.
pub const RECALL_DEFAULT_RESULTS: u8 = 4;

/// Per-turn cap on how many `recall` tool calls a single turn may make.
/// Sized so a normal turn that legitimately wants context can issue
/// several queries without runaway looping.
pub const MAX_RECALL_CALLS_PER_TURN: usize = 6;

/// Cosine-similarity threshold above which a pair of memories is
/// considered "the same content" for dedup.
///
/// Calibrated for `text-embedding-3-small`; at 0.95 a typo-difference
/// reliably matches while two distinct beliefs do not.
pub const DEDUP_SIMILARITY_THRESHOLD: f32 = 0.95;

/// Cosine-similarity threshold above which a pair of memories is a
/// candidate for contradiction detection (subject to a heuristic textual
/// signal of opposition — see `librarian::contradicts`).
pub const CONTRADICTION_SIMILARITY_THRESHOLD: f32 = 0.85;

/// Hard cap on pairs returned by [`MemoryStore::similar_pairs`] per
/// agent per sweep. Bounded because a quadratic scan over an agent at
/// quota would otherwise return up to ~5e5 pairs.
pub const MAX_SIMILAR_PAIRS_PER_AGENT: usize = 256;

/// Age threshold beyond which a `Validated` memory decays back to `Held`.
///
/// (doc/memory.md §1.7) Long enough that a memory validated today still
/// surfaces the same way next month; short enough that an agent that
/// hasn't seen evidence of a memory in a while stops trusting it.
pub const VALIDATION_DECAY: chrono::Duration = chrono::Duration::seconds(60 * 60 * 24 * 30);

/// Age threshold for passive `Tentative → Held` maturation (doc/memory.md §1.8).
///
/// State-only promotion — `last_validated_at` is *not* advanced; only the
/// state field moves. The promotion is one-step: `Tentative → Held`. Reaching
/// `Validated` still requires an independent signal (`memory_validate`,
/// operator endorsement).
///
/// Long enough that a fresh write survives normal "the user changed their
/// mind in the next reply" churn before hardening; short enough that
/// internal-only beliefs (preferences the agent cannot externally verify)
/// can leave `Tentative` before they decay out under quota.
pub const MATURATION_WINDOW: chrono::Duration = chrono::Duration::seconds(60 * 60 * 24 * 7);

/// Polling cadence for the librarian sweep.
///
/// Sized longer than the reflection cadence — librarian work is heavier
/// (per-pair queries) and only needs to catch up overnight. Bounded so
/// a wedged scheduler cannot hammer the database.
pub const LIBRARIAN_POLL_SECS: u64 = 60 * 60;

/// Cap on how many agents a single librarian tick processes. Bounded
/// so a fleet of agents does not produce one gigantic transaction.
pub const LIBRARIAN_BATCH_LIMIT: usize = 32;

/// Cap on how many agent rows the operator audit endpoint returns per
/// page. Bounds the worst-case payload size.
pub const OPERATOR_AUDIT_PAGE_LIMIT: usize = 256;
