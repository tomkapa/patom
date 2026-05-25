//! Newtypes for the agent todo list (CLAUDE.md §1: parse, don't validate).
//!
//! The model speaks JSON; values funnel through `TryFrom` on the way in
//! and `Serialize` on the way out. There is no other way to construct a
//! `TodoId` / `TodoContent` / `TodoList` than through these gates.

use std::collections::HashSet;
use std::fmt;

use serde::{Deserialize, Serialize};

use crate::types::ParseError;

use super::limits::{MAX_TODO_CONTENT_BYTES, MAX_TODO_ID_BYTES, MAX_TODOS_PER_LIST};

/// Stable, model-supplied handle for a single todo within its session's
/// list. Short string (≤ `MAX_TODO_ID_BYTES`) — the model uses it to
/// refer back to the same item across turns ("mark T-1 completed").
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
#[serde(transparent)]
pub struct TodoId(String);

impl TodoId {
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for TodoId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl TryFrom<String> for TodoId {
    type Error = ParseError;
    fn try_from(raw: String) -> Result<Self, Self::Error> {
        if raw.is_empty() {
            return Err(ParseError::Empty { field: "todo.id" });
        }
        if raw.len() > MAX_TODO_ID_BYTES {
            return Err(ParseError::TooLong {
                field: "todo.id",
                max: MAX_TODO_ID_BYTES,
                got: raw.len(),
            });
        }
        if !raw
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        {
            return Err(ParseError::Malformed {
                field: "todo.id",
                detail: "only [a-zA-Z0-9_-] permitted",
            });
        }
        Ok(Self(raw))
    }
}

// Boundary deserialiser: deserialise the raw `String`, funnel through
// `TryFrom` (CLAUDE.md §1 — serde never sidesteps the smart constructor).
impl<'de> Deserialize<'de> for TodoId {
    fn deserialize<D>(d: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let raw = String::deserialize(d)?;
        Self::try_from(raw).map_err(serde::de::Error::custom)
    }
}

/// Human-readable description of one task. Bounded to
/// [`MAX_TODO_CONTENT_BYTES`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct TodoContent(String);

impl TodoContent {
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for TodoContent {
    type Error = ParseError;
    fn try_from(raw: String) -> Result<Self, Self::Error> {
        if raw.is_empty() {
            return Err(ParseError::Empty {
                field: "todo.content",
            });
        }
        if raw.len() > MAX_TODO_CONTENT_BYTES {
            return Err(ParseError::TooLong {
                field: "todo.content",
                max: MAX_TODO_CONTENT_BYTES,
                got: raw.len(),
            });
        }
        Ok(Self(raw))
    }
}

impl<'de> Deserialize<'de> for TodoContent {
    fn deserialize<D>(d: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let raw = String::deserialize(d)?;
        Self::try_from(raw).map_err(serde::de::Error::custom)
    }
}

/// Lifecycle of a single todo.
///
/// Three states, matching Claude Code's TodoWrite — `in_progress` is
/// the model's signal of "what I'm doing right now". CLAUDE.md §1:
/// enum over `bool` + `Option`, exhaustive match on every consumer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TodoStatus {
    Pending,
    InProgress,
    Completed,
}

impl TodoStatus {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::InProgress => "in_progress",
            Self::Completed => "completed",
        }
    }
}

/// One item in the agent's todo list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TodoItem {
    pub id: TodoId,
    pub content: TodoContent,
    pub status: TodoStatus,
}

/// A validated todo list. Construct via [`Self::try_from`] — that is the
/// only path that enforces:
///
/// * cardinality ≤ [`MAX_TODOS_PER_LIST`]
/// * IDs unique within the list
/// * at most one item in [`TodoStatus::InProgress`] — the model picks
///   one focus at a time, mirroring Claude Code's TodoWrite contract.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct TodoList(Vec<TodoItem>);

impl TodoList {
    #[must_use]
    pub fn as_slice(&self) -> &[TodoItem] {
        &self.0
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    #[must_use]
    pub fn into_inner(self) -> Vec<TodoItem> {
        self.0
    }

    /// Empty list — sentinel for "no todos written yet for this session".
    /// Skips the constructor's invariant checks because both invariants
    /// hold trivially on `vec![]`.
    #[must_use]
    pub fn empty() -> Self {
        Self(Vec::new())
    }
}

impl TryFrom<Vec<TodoItem>> for TodoList {
    type Error = ParseError;
    fn try_from(items: Vec<TodoItem>) -> Result<Self, Self::Error> {
        if items.len() > MAX_TODOS_PER_LIST {
            return Err(ParseError::TooLong {
                field: "todo.items",
                max: MAX_TODOS_PER_LIST,
                got: items.len(),
            });
        }
        let mut seen: HashSet<&str> = HashSet::with_capacity(items.len());
        let mut in_progress = 0usize;
        for item in &items {
            if !seen.insert(item.id.as_str()) {
                return Err(ParseError::Malformed {
                    field: "todo.items",
                    detail: "duplicate id",
                });
            }
            if item.status == TodoStatus::InProgress {
                in_progress += 1;
            }
        }
        if in_progress > 1 {
            return Err(ParseError::Malformed {
                field: "todo.items",
                detail: "at most one item may be in_progress",
            });
        }
        Ok(Self(items))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(id: &str, status: TodoStatus) -> TodoItem {
        TodoItem {
            id: TodoId::try_from(id.to_owned()).expect("valid id"),
            content: TodoContent::try_from("do a thing".to_owned()).expect("valid content"),
            status,
        }
    }

    #[test]
    fn rejects_more_than_one_in_progress() {
        let items = vec![
            item("a", TodoStatus::InProgress),
            item("b", TodoStatus::InProgress),
        ];
        let err = TodoList::try_from(items).expect_err("two in_progress should reject");
        assert!(matches!(err, ParseError::Malformed { .. }));
    }

    #[test]
    fn rejects_duplicate_ids() {
        let items = vec![
            item("a", TodoStatus::Pending),
            item("a", TodoStatus::Completed),
        ];
        let err = TodoList::try_from(items).expect_err("dup ids should reject");
        assert!(matches!(err, ParseError::Malformed { .. }));
    }

    #[test]
    fn rejects_over_capacity() {
        let items: Vec<TodoItem> = (0..=MAX_TODOS_PER_LIST)
            .map(|i| item(&format!("t{i}"), TodoStatus::Pending))
            .collect();
        let err = TodoList::try_from(items).expect_err("over-cap should reject");
        assert!(matches!(err, ParseError::TooLong { .. }));
    }

    #[test]
    fn list_invariants_excerpt() {
        // sanity: a single in_progress is fine; this exercises the
        // accept-side of the at-most-one check.
        let items = vec![item("focus", TodoStatus::InProgress)];
        assert!(TodoList::try_from(items).is_ok());
    }

    #[test]
    fn accepts_one_in_progress_and_many_pending() {
        let items = vec![
            item("a", TodoStatus::InProgress),
            item("b", TodoStatus::Pending),
            item("c", TodoStatus::Completed),
        ];
        let list = TodoList::try_from(items).expect("valid list");
        assert_eq!(list.len(), 3);
    }

    #[test]
    fn id_rejects_invalid_chars() {
        assert!(TodoId::try_from("a b".to_owned()).is_err());
        assert!(TodoId::try_from("emoji-😀".to_owned()).is_err());
    }

    #[test]
    fn id_rejects_empty_and_oversize() {
        assert!(TodoId::try_from(String::new()).is_err());
        let too_long = "a".repeat(MAX_TODO_ID_BYTES + 1);
        assert!(TodoId::try_from(too_long).is_err());
    }

    #[test]
    fn content_rejects_empty_and_oversize() {
        assert!(TodoContent::try_from(String::new()).is_err());
        let too_long = "a".repeat(MAX_TODO_CONTENT_BYTES + 1);
        assert!(TodoContent::try_from(too_long).is_err());
    }
}
