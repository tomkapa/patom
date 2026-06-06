//! `memory_update` — revises an existing memory's content.

use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::memory::{
    MemoryContent, MemoryHandle, MemoryId, MemoryMutation, MemoryState, MutationSource,
};
use crate::tools::{Tool, ToolCallContext, ToolError};
use crate::types::ToolName;

use super::{
    MemoryToolDeps, check_cap, expect_agent, maybe_close_resolution, parse_to_tool_err,
    resolve_handle, store_to_tool_err,
};

const TOOL_NAME: &str = "memory_update";

const TOOL_DESCRIPTION: &str = "Revise an existing memory by handle. Use when a stored memory is wrong or \
     stale — \"your memory M-12 is wrong, the deadline is Tuesday\".\n\
     \n\
     The handle (e.g. `M-12`) comes from the `## Memory` section in your system \
     prompt. Updating resets the memory to `tentative` (a content change is, by \
     definition, unverified again). Pinned memories reject agent updates.\n\
     \n\
     Arguments: `handle` (the `M-NN` form), `content` (one or two sentences, max \
     4096 bytes).";

#[derive(Debug, Deserialize)]
struct Input {
    handle: String,
    content: String,
}

#[derive(Debug, Serialize)]
struct Output {
    memory_id: MemoryId,
    handle: String,
    state: &'static str,
}

pub struct MemoryUpdateTool {
    name: ToolName,
    description: &'static str,
    input_schema: Arc<Value>,
    deps: MemoryToolDeps,
}

impl std::fmt::Debug for MemoryUpdateTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MemoryUpdateTool").finish_non_exhaustive()
    }
}

impl MemoryUpdateTool {
    #[must_use]
    pub fn new(deps: MemoryToolDeps) -> Self {
        let name = ToolName::try_from(TOOL_NAME).expect("valid tool name");
        let input_schema = Arc::new(json!({
            "type": "object",
            "required": ["handle", "content"],
            "properties": {
                "handle": { "type": "string", "pattern": "^M-[0-9]+$" },
                "content": { "type": "string", "minLength": 1, "maxLength": 4096 }
            },
            "additionalProperties": false,
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
impl Tool for MemoryUpdateTool {
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
        let parsed: Input = serde_json::from_value(input)?;
        let agent = expect_agent(ctx)?;
        check_cap(&self.deps.counter, ctx.request_id)?;

        let handle = MemoryHandle::try_from(parsed.handle.as_str()).map_err(parse_to_tool_err)?;
        let memory_id =
            resolve_handle(&self.deps, ctx.session_id, agent, &ctx.kind_payload, handle).await?;
        let content = MemoryContent::try_from(parsed.content).map_err(parse_to_tool_err)?;

        let outcome = self
            .deps
            .store()
            .apply_for_user(
                ctx.acting_user_id,
                MemoryMutation::Update {
                    agent,
                    target: memory_id,
                    content,
                    state: MemoryState::Tentative,
                    source: MutationSource::Turn(ctx.request_id),
                },
            )
            .await
            .map_err(store_to_tool_err)?;

        maybe_close_resolution(&self.deps, ctx, outcome.event_id).await?;

        let out = Output {
            memory_id: outcome.memory_id,
            handle: handle.to_string(),
            state: MemoryState::Tentative.as_str(),
        };
        Ok(serde_json::to_string(&out)?)
    }
}
