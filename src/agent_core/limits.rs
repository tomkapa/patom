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

/// Per-call timeout for `LlmProvider::send`. CLAUDE.md §5: every I/O await is wrapped.
pub const PROVIDER_CALL_TIMEOUT: Duration = Duration::from_secs(120);

/// Per-call timeout for any `Tool::execute`. The tool may have its own narrower timeout
/// (e.g. fetch is 20 s); this is the outer fence.
pub const TOOL_CALL_TIMEOUT: Duration = Duration::from_secs(60);

/// Per-turn ceiling on the pre-turn `session_todos` read.
///
/// CLAUDE.md §5: every I/O await is wrapped. This read is a PK lookup
/// against a single row — under healthy conditions it returns in
/// milliseconds. 2 s is generous for a transient connection-pool
/// stall while still keeping the turn's critical path bounded.
pub(super) const TODOS_LOAD_TIMEOUT: Duration = Duration::from_secs(2);

// §5: defaults must always parse cleanly through their newtype constructors. Pinned at
// compile time so a future bump cannot silently invert the relationship.
const _: () = assert!(DEFAULT_MAX_OUTPUT_TOKENS > 0);
const _: () = assert!(DEFAULT_MAX_OUTPUT_TOKENS <= MAX_OUTPUT_TOKENS_CAP);
const _: () = assert!(DEFAULT_MAX_TURNS > 0);
const _: () = assert!(DEFAULT_MAX_TURNS <= MAX_TURNS_CAP);
