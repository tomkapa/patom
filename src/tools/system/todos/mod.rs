//! `todo_write` — durable per-session task list for the agent.
//!
//! One tool, one table. The model passes the full intended list each
//! call and the handler replaces the prior row atomically (Claude
//! Code's TodoWrite semantics). The list survives across turns and
//! re-runs of the same session — `super::super::super::agent_core`
//! folds the rendered list into the system prompt at the top of every
//! turn so the model sees its own state coming in (see
//! [`render_section`]).

use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::Mutex;

use crate::runtime::PromptRequestId;

use super::super::traits::ToolError;

mod limits;
mod store;
mod types;
mod write;

pub use limits::{
    MAX_TODO_CONTENT_BYTES, MAX_TODO_ID_BYTES, MAX_TODO_WRITES_PER_TURN, MAX_TODOS_PER_LIST,
};
pub use store::{PgSessionTodoStore, SessionTodoStore, SharedSessionTodoStore, TodoStoreError};
pub use types::{TodoContent, TodoId, TodoItem, TodoList, TodoStatus};
pub use write::{TodoToolDeps, TodoWriteTool};

/// Per-turn call counter for `todo_write`.
///
/// Same bounded-HashMap shape as the memory tool's counter
/// (CLAUDE.md §5): once a turn hits its cap, further calls return
/// [`CapExceeded`]; the map self-evicts when it gets too wide so a
/// long-running process can't leak. Held under [`TodoToolDeps`] so a
/// hand-rolled test can swap the counter for one with a low cap.
#[derive(Debug)]
pub struct PerTurnCallCounter {
    inner: Mutex<HashMap<PromptRequestId, usize>>,
    cap_per_turn: usize,
    bookkeeping_max_entries: usize,
}

impl PerTurnCallCounter {
    #[must_use]
    pub fn with_cap(cap_per_turn: usize) -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
            cap_per_turn,
            bookkeeping_max_entries: 1024,
        }
    }

    pub fn try_increment(&self, request_id: PromptRequestId) -> Result<usize, CapExceeded> {
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
pub struct CapExceeded {
    pub cap: usize,
}

pub(super) fn check_cap(
    counter: &PerTurnCallCounter,
    request_id: PromptRequestId,
) -> Result<(), ToolError> {
    counter.try_increment(request_id).map(|_| ()).map_err(|e| {
        ToolError::InvalidInput(format!(
            "todo_write cap exceeded for this turn (max {} writes)",
            e.cap
        ))
    })
}

/// Render the session's current todo list as a `<todos>` block.
///
/// Returns an empty string when the list is empty so the renderer can
/// omit the envelope entirely (no wasted prompt budget on
/// `<todos></todos>`). Called from
/// `agent_core::turn::build_chat_request` right after the memory
/// composer's system prompt — this is the load-bearing piece that
/// makes "persists across re-runs" visible to the model. Without it
/// the row exists in Postgres but the model never sees it.
#[must_use]
pub fn render_section(list: &TodoList) -> String {
    if list.is_empty() {
        return String::new();
    }
    let items = list.as_slice();
    // bounded; capacity guess of 96 B/line accounts for id + status +
    // typical content length without exceeding MAX_TODOS_PER_LIST × that.
    let mut out = String::with_capacity(items.len() * 96 + 32);
    out.push_str("<todos>\n");
    for item in items {
        // `write!` into a String is infallible; the result is ignored
        // by design (CLAUDE.md §6 — assertions, not Result handling,
        // catch impossible failures here).
        let _ = writeln!(
            out,
            "- [{}] ({}) {}",
            item.status.as_str(),
            item.id.as_str(),
            item.content.as_str()
        );
    }
    out.push_str("</todos>");
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ParseError;

    #[test]
    fn counter_caps_at_per_turn_limit() {
        let counter = PerTurnCallCounter::with_cap(3);
        let req = PromptRequestId::new();
        for i in 1..=3 {
            assert_eq!(counter.try_increment(req).expect("under cap"), i);
        }
        assert!(counter.try_increment(req).is_err());
    }

    #[test]
    fn counter_is_per_request() {
        let counter = PerTurnCallCounter::with_cap(1);
        let r1 = PromptRequestId::new();
        let r2 = PromptRequestId::new();
        counter.try_increment(r1).expect("r1 ok");
        counter.try_increment(r2).expect("r2 ok");
        assert!(counter.try_increment(r1).is_err());
        assert!(counter.try_increment(r2).is_err());
    }

    fn make_item(id: &str, content: &str, status: TodoStatus) -> TodoItem {
        TodoItem {
            id: TodoId::try_from(id.to_owned()).expect("valid id"),
            content: TodoContent::try_from(content.to_owned()).expect("valid content"),
            status,
        }
    }

    #[test]
    fn render_empty_list_is_empty_string() {
        assert!(render_section(&TodoList::empty()).is_empty());
    }

    #[test]
    fn render_includes_all_items_in_order() {
        let list = TodoList::try_from(vec![
            make_item("a", "first", TodoStatus::Completed),
            make_item("b", "second", TodoStatus::InProgress),
            make_item("c", "third", TodoStatus::Pending),
        ])
        .expect("valid list");
        let rendered = render_section(&list);
        assert!(rendered.starts_with("<todos>\n"));
        assert!(rendered.ends_with("</todos>"));
        assert!(rendered.contains("[completed] (a) first"));
        assert!(rendered.contains("[in_progress] (b) second"));
        assert!(rendered.contains("[pending] (c) third"));
        // order preserved
        let a_pos = rendered.find("(a)").expect("a present");
        let b_pos = rendered.find("(b)").expect("b present");
        let c_pos = rendered.find("(c)").expect("c present");
        assert!(a_pos < b_pos);
        assert!(b_pos < c_pos);
    }

    #[test]
    fn parse_error_round_trip_smoke() {
        // belt-and-braces: TodoList's `try_from` surfaces a ParseError
        // variant the tool's `store_to_tool_err` knows how to translate.
        let err = TodoList::try_from(vec![
            make_item("x", "one", TodoStatus::InProgress),
            make_item("y", "two", TodoStatus::InProgress),
        ])
        .expect_err("two in_progress should reject");
        assert!(matches!(err, ParseError::Malformed { .. }));
    }
}
