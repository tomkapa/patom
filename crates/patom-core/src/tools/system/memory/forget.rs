//! `memory_forget` — removes a memory from the materialized view.

use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::memory::{MemoryHandle, MemoryId, MemoryMutation, MutationSource};
use crate::tools::{Tool, ToolCallContext, ToolError};
use crate::types::ToolName;

use super::{
    MemoryToolDeps, check_cap, expect_agent, maybe_close_resolution, parse_to_tool_err,
    resolve_handle, store_to_tool_err,
};

const TOOL_NAME: &str = "memory_forget";

const TOOL_DESCRIPTION: &str = "Remove an existing memory by handle. Use when a stored memory is obsolete or \
     the user asks you to forget it — \"forget M-44, that's no longer how we do \
     it\". The journal retains the event so an operator can revert; the memory \
     stops appearing in your system prompt immediately. Pinned memories reject \
     agent forgets.\n\
     \n\
     Arguments: `handle` (the `M-NN` form).";

#[derive(Debug, Deserialize)]
struct Input {
    handle: String,
}

#[derive(Debug, Serialize)]
struct Output {
    memory_id: MemoryId,
    handle: String,
    status: &'static str,
}

pub struct MemoryForgetTool {
    name: ToolName,
    description: &'static str,
    input_schema: Arc<Value>,
    deps: MemoryToolDeps,
}

impl std::fmt::Debug for MemoryForgetTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MemoryForgetTool").finish_non_exhaustive()
    }
}

impl MemoryForgetTool {
    #[must_use]
    pub fn new(deps: MemoryToolDeps) -> Self {
        let name = ToolName::try_from(TOOL_NAME).expect("valid tool name");
        let input_schema = Arc::new(json!({
            "type": "object",
            "required": ["handle"],
            "properties": {
                "handle": { "type": "string", "pattern": "^M-[0-9]+$" }
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
impl Tool for MemoryForgetTool {
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

        let outcome = self
            .deps
            .store()
            .apply_for_user(
                ctx.acting_user_id,
                MemoryMutation::Forget {
                    agent,
                    target: memory_id,
                    source: MutationSource::Turn(ctx.request_id),
                },
            )
            .await
            .map_err(store_to_tool_err)?;

        maybe_close_resolution(&self.deps, ctx, outcome.event_id).await?;

        let out = Output {
            memory_id: outcome.memory_id,
            handle: handle.to_string(),
            status: "forgotten",
        };
        Ok(serde_json::to_string(&out)?)
    }
}
