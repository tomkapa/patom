//! `todo_write` — atomic overwrite of the agent's per-session todo list.
//!
//! Mirrors Claude Code's TodoWrite: one call replaces the whole list.
//! The model is expected to keep exactly one item `in_progress` and to
//! re-issue the full list on every change. The handler echoes the
//! stored list back as the tool result so the model sees its own state.

use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tracing::{Instrument, debug, error, info_span};

use crate::tools::{Tool, ToolCallContext, ToolError};
use crate::types::ToolName;

use super::limits::{
    MAX_TODO_CONTENT_BYTES, MAX_TODO_ID_BYTES, MAX_TODO_WRITES_PER_TURN, MAX_TODOS_PER_LIST,
};
use super::store::{SharedSessionTodoStore, TodoStoreError};
use super::types::{TodoItem, TodoList};
use super::{PerTurnCallCounter, check_cap};

const TOOL_NAME: &str = "todo_write";

const TOOL_DESCRIPTION: &str = "Maintain a durable, per-session task list for multi-step work. \
     Pass the FULL desired list each call — the tool replaces the prior list atomically. \
     The list persists across turns and re-runs of this session, so use it for plans \
     that span several model responses.\n\
     \n\
     USE WHEN: a request has 3+ distinct steps, a non-trivial bug fix, or any work \
     where you want a visible \"what I'm doing now\" signal. Mark exactly ONE item \
     `in_progress` while you work on it. Mark items `completed` as soon as they're \
     truly done. DO NOT use for single-step requests or pure conversation.\n\
     \n\
     Arguments: `items` is an array of `{ id, content, status }`. `id` is a short \
     stable string you invent ([a-zA-Z0-9_-], ≤32 bytes) — keep it stable across \
     calls so the same task keeps the same id. `content` is one short sentence \
     (≤512 bytes). `status` is one of \"pending\", \"in_progress\", \"completed\".";

#[derive(Debug, Clone)]
pub struct TodoToolDeps {
    pub store: SharedSessionTodoStore,
    pub counter: Arc<PerTurnCallCounter>,
}

impl TodoToolDeps {
    #[must_use]
    pub fn new(store: SharedSessionTodoStore) -> Self {
        Self {
            store,
            counter: Arc::new(PerTurnCallCounter::with_cap(MAX_TODO_WRITES_PER_TURN)),
        }
    }
}

#[derive(Debug, Deserialize)]
struct Input {
    items: Vec<TodoItem>,
}

#[derive(Debug, Serialize)]
struct Output {
    items: TodoList,
    count: usize,
    note: &'static str,
}

pub struct TodoWriteTool {
    name: ToolName,
    description: &'static str,
    input_schema: Arc<Value>,
    deps: TodoToolDeps,
}

impl std::fmt::Debug for TodoWriteTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TodoWriteTool").finish_non_exhaustive()
    }
}

impl TodoWriteTool {
    #[must_use]
    pub fn new(deps: TodoToolDeps) -> Self {
        let name = ToolName::try_from(TOOL_NAME).expect("invariant: TOOL_NAME is valid");
        let input_schema = Arc::new(json!({
            "type": "object",
            "required": ["items"],
            "additionalProperties": false,
            "properties": {
                "items": {
                    "type": "array",
                    "maxItems": MAX_TODOS_PER_LIST,
                    "items": {
                        "type": "object",
                        "required": ["id", "content", "status"],
                        "additionalProperties": false,
                        "properties": {
                            "id": {
                                "type": "string",
                                "minLength": 1,
                                "maxLength": MAX_TODO_ID_BYTES,
                                "pattern": "^[a-zA-Z0-9_-]+$",
                            },
                            "content": {
                                "type": "string",
                                "minLength": 1,
                                "maxLength": MAX_TODO_CONTENT_BYTES,
                            },
                            "status": {
                                "type": "string",
                                "enum": ["pending", "in_progress", "completed"],
                            },
                        },
                    },
                },
            },
        }));
        Self {
            name,
            description: TOOL_DESCRIPTION,
            input_schema,
            deps,
        }
    }
}

#[async_trait]
impl Tool for TodoWriteTool {
    fn name(&self) -> &ToolName {
        &self.name
    }
    fn description(&self) -> &str {
        self.description
    }
    fn input_schema(&self) -> Arc<Value> {
        self.input_schema.clone()
    }
    async fn execute(&self, input: Value, ctx: &ToolCallContext) -> Result<String, ToolError> {
        // CLAUDE.md §2: externally-triggered unit of work opens its own
        // span. Low-cardinality name, dynamic values on fields.
        let span = info_span!(
            "tool.todo_write",
            relay.session.id = %ctx.session_id,
            relay.request.id = %ctx.request_id,
            relay.todo.count = tracing::field::Empty,
        );
        async move {
            let parsed: Input = serde_json::from_value(input).map_err(|e| {
                error!(event = "todo_write.invalid_json", error = ?e);
                ToolError::from(e)
            })?;
            if let Err(e) = check_cap(&self.deps.counter, ctx.request_id) {
                error!(event = "todo_write.cap_exceeded", error = ?e);
                return Err(e);
            }
            let list = TodoList::try_from(parsed.items).map_err(|e| {
                error!(event = "todo_write.invariant_rejected", error = ?e);
                ToolError::InvalidInput(e.to_string())
            })?;
            let count = list.len();
            tracing::Span::current().record("relay.todo.count", count);
            let stored = self
                .deps
                .store
                .replace(
                    ctx.acting_user_id,
                    ctx.session_id,
                    ctx.org_id,
                    ctx.request_id,
                    list,
                )
                .await
                .map_err(|e| {
                    error!(event = "todo_write.store_error", error = ?e);
                    store_to_tool_err(e)
                })?;

            debug!(event = "todo_write.ok", relay.todo.count = count);

            let out = Output {
                items: stored,
                count,
                note: "Todo list saved. It will be re-shown to you at the top of every \
                       future turn in this session.",
            };
            Ok(serde_json::to_string(&out)?)
        }
        .instrument(span)
        .await
    }
}

fn store_to_tool_err(e: TodoStoreError) -> ToolError {
    match e {
        TodoStoreError::Invariant(p) => ToolError::InvalidInput(p.to_string()),
        TodoStoreError::Db(d) => ToolError::Backend(d.to_string()),
    }
}
