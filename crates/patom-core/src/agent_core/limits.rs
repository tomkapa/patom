//! Agent invariants. Per CLAUDE.md §5: every limit is named, doc-commented, and
//! exported so the operator can audit them in one place.

use std::time::Duration;

use crate::types::{MAX_OUTPUT_TOKENS_CAP, MAX_TURNS_CAP};

/// Default model output budget per turn. Comfortably under typical model caps; bumped
/// per-Agent via the builder when a tool-heavy task warrants it.
pub const DEFAULT_MAX_OUTPUT_TOKENS: u32 = 4096;

/// Default tool/turn iterations per `Agent::reply`. Above this the agent gives up rather
/// than letting a model loop on a stuck plan.
pub const DEFAULT_MAX_TURNS: u32 = 12;

/// Hard cap on tool calls inside a single assistant turn. Defends against a model that
/// fans out an unreasonable number of parallel calls.
pub const MAX_TOOL_CALLS_PER_TURN: usize = 16;

/// Hard cap on hook evaluations recorded for a single turn.
///
/// Defends the per-turn drawer join (`/turns/:id`) — `hook_events` lands in slice
/// 3 but the cap belongs next to the tool-call cap so the join knows its ceiling
/// today.
pub const MAX_HOOKS_PER_TURN: usize = 32;

/// Per-call timeout for `LlmProvider::send`. CLAUDE.md §5: every I/O await is wrapped.
pub const PROVIDER_CALL_TIMEOUT: Duration = Duration::from_mins(2);

/// Per-call timeout for any `Tool::execute`. The tool may have its own narrower timeout
/// (e.g. fetch is 20 s); this is the outer fence.
pub const TOOL_CALL_TIMEOUT: Duration = Duration::from_mins(1);

/// Hard cap on the number of `turn_metrics` rows aggregated into one
/// `/agents/:id/metrics/timeseries` response.
///
/// CLAUDE.md §5: every batch has a size cap. Sized for a 30-day window
/// where an agent runs ~150 turns/day — leaves a healthy safety margin
/// while keeping a single rollup query bounded.
pub const MAX_TURNS_PER_TIMESERIES_RESPONSE: i64 = 5_000;

/// Maximum page size for `/agents/:id/turns?cursor=…`. Matches the
/// frontend's `useInfiniteQuery` page size; bounded so a misbehaving
/// client can't ask for a megapage.
pub const MAX_TURN_LIST_PAGE_SIZE: u16 = 50;

/// Per-turn ceiling on the pre-turn `session_todos` read.
///
/// CLAUDE.md §5: every I/O await is wrapped. This read is a PK lookup
/// against a single row — under healthy conditions it returns in
/// milliseconds. 2 s is generous for a transient connection-pool
/// stall while still keeping the turn's critical path bounded.
pub(super) const TODOS_LOAD_TIMEOUT: Duration = Duration::from_secs(2);

// --- Context compaction (#182) -------------------------------------------

/// Divisor turning a model's context window into the per-turn prompt token budget.
///
/// `budget = context_window / DIVISOR`. 2 (= 50%) leaves the other half for the
/// system prompt, tool schemas, the rolling summary, and the output — the trigger
/// fires before the window is actually full.
pub const CONTEXT_TOKEN_BUDGET_DIVISOR: u32 = 2;

/// Per-chunk input budget (estimated tokens) handed to one summarizer fold.
///
/// Bounds a single fold's prompt so even a huge overflow is digested in bounded
/// pieces; ~24k tokens is a comfortable single call for any catalog model while
/// keeping each fold cheap.
pub const SUMMARIZER_INPUT_BUDGET: u32 = 24_000;

/// Hard cap on summarizer folds in one compaction (CLAUDE.md §5 — every loop is bounded).
///
/// Steady state is 1–2 folds (an overflow is ~one window); this only bites on the
/// first compaction of a pre-existing huge thread, where the oldest-beyond-cap
/// chunks are dropped with a WARN + saturation metric.
pub const MAX_COMPACTION_CHUNKS: usize = 8;

/// Per-fold timeout.
///
/// Every summarizer `await` is wrapped (CLAUDE.md §5); on expiry the turn falls
/// back to the windowing floor.
pub const COMPACTION_LLM_TIMEOUT: Duration = Duration::from_secs(30);

/// Total wall-clock budget across *all* folds in one inline compaction.
///
/// Because folds are rolling (sequential, can't parallelize), the per-fold timeout
/// alone could stack to minutes on a cold thread; this caps the whole pass so a
/// turn never stalls unboundedly. On exhaustion the turn serves the floor + the
/// partial/stale summary.
pub const MAX_COMPACTION_WALL_CLOCK: Duration = Duration::from_secs(90);

/// How long to skip re-summarizing a `(thread, agent)` after a summarizer failure.
///
/// Without it a persistently-failing summarizer would re-attempt (and re-pay the
/// timeout) on every turn; the cooldown degrades it to "just the floor" until it
/// clears.
pub const COMPACTION_COOLDOWN: Duration = Duration::from_mins(5);

/// Cap on the rolling summary's own size (estimated tokens) so it can't grow unbounded.
///
/// Enforced by `CompactionSummary` (truncates to fit).
pub const MAX_SUMMARY_TOKENS: u32 = 4_000;

// §5: defaults must always parse cleanly through their newtype constructors. Pinned at
// compile time so a future bump cannot silently invert the relationship.
const _: () = assert!(DEFAULT_MAX_OUTPUT_TOKENS > 0);
const _: () = assert!(DEFAULT_MAX_OUTPUT_TOKENS <= MAX_OUTPUT_TOKENS_CAP);
const _: () = assert!(DEFAULT_MAX_TURNS > 0);
const _: () = assert!(DEFAULT_MAX_TURNS <= MAX_TURNS_CAP);
const _: () = assert!(CONTEXT_TOKEN_BUDGET_DIVISOR > 0);
const _: () = assert!(MAX_COMPACTION_CHUNKS > 0);
