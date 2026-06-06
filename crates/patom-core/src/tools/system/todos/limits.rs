//! Todo-tool invariants. Per CLAUDE.md §5: named, doc-commented, with *why*.

/// Maximum todos a single list can hold.
///
/// Claude Code's TodoWrite keeps lists short on purpose — the model
/// works through a focused backlog, not a wall of items. 50 covers the
/// largest real plans we've seen and matches the DB CHECK on
/// `session_todos.item_count` so app and database refuse oversized
/// lists consistently.
pub const MAX_TODOS_PER_LIST: usize = 50;

/// Maximum bytes for a single todo `content` field.
///
/// 512 bytes is one sentence of plan English. Anything longer is a
/// design note that belongs in agent messages, not a backlog item.
pub const MAX_TODO_CONTENT_BYTES: usize = 512;

/// Maximum bytes for a model-supplied todo `id`.
///
/// IDs are short stable handles the model invents to refer back to its
/// own list across turns ("1", "research-step", "T-3"). 32 bytes is
/// generous for any sane scheme without inviting freeform note-taking
/// in the ID field.
pub const MAX_TODO_ID_BYTES: usize = 32;

/// Maximum `todo_write` invocations per single `prompt_request` (turn).
///
/// The tool overwrites the full list each call; a sane agent calls it
/// at most a handful of times per turn (mark in-progress, mark
/// completed, add a discovered subtask). 20 is well above honest use
/// and well below "agent stuck in a write loop".
pub const MAX_TODO_WRITES_PER_TURN: usize = 20;
