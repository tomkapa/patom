//! Session-store invariants. CLAUDE.md §5: every container has an explicit upper bound.

/// Maximum number of messages stored in a single session.
///
/// Each turn produces 1 user message (the prompt or the tool results) plus 1 assistant
/// message; this caps a session at ~256 turns including pathological tool loops. Beyond
/// this we refuse to grow the history rather than letting one runaway session eat a host.
pub const MAX_MESSAGES_PER_SESSION: u32 = 512;
