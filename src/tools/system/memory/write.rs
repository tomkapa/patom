//! `memory_write` — mints a new agent-owned memory.

use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tracing::debug;

use crate::memory::{
    MemoryContent, MemoryId, MemoryKind, MemoryMutation, MemoryState, MutationSource,
};
use crate::tools::{Tool, ToolCallContext, ToolError};
use crate::types::ToolName;

use super::{MemoryToolDeps, check_cap, expect_agent, parse_to_tool_err, store_to_tool_err};

const TOOL_NAME: &str = "memory_write";

const TOOL_DESCRIPTION: &str = "Persist a new memory the agent should carry across sessions. \
     Memory is the agent's distilled understanding of itself, its peers, and learned \
     procedures — not raw conversation transcript.\n\
     \n\
     USE WHEN the user (or a peer agent) explicitly asks you to remember something — \
     \"remember I prefer tabs over spaces\", \"keep in mind we deploy on Mondays\". \
     DO NOT use on weak or implicit signals; reflection turns capture those.\n\
     \n\
     Arguments: `kind` is one of \"self\" (identity, style, preferences), \"other\" \
     (beliefs about specific peers or humans), \"collaborator\" (beliefs about \
     other agents in your network, written after a successful delegation), \
     \"procedure\" (learned how-tos), \"open\" (known unknowns); `content` is one \
     or two sentences (max 4096 bytes). New memories land at `tentative` state \
     and need independent confirmation to promote.";

#[derive(Debug, Deserialize)]
struct Input {
    kind: MemoryKind,
    content: String,
}

#[derive(Debug, Serialize)]
struct Output {
    memory_id: MemoryId,
    kind: &'static str,
    state: &'static str,
    note: &'static str,
}

pub struct MemoryWriteTool {
    name: ToolName,
    description: &'static str,
    input_schema: Arc<Value>,
    deps: MemoryToolDeps,
}

impl std::fmt::Debug for MemoryWriteTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MemoryWriteTool").finish_non_exhaustive()
    }
}

impl MemoryWriteTool {
    #[must_use]
    pub fn new(deps: MemoryToolDeps) -> Self {
        let name = ToolName::try_from(TOOL_NAME).expect("valid tool name");
        let input_schema = Arc::new(json!({
            "type": "object",
            "required": ["kind", "content"],
            "properties": {
                "kind": { "type": "string", "enum": ["self", "other", "collaborator", "procedure", "open"] },
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
impl Tool for MemoryWriteTool {
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

        let content = MemoryContent::try_from(parsed.content).map_err(parse_to_tool_err)?;
        let outcome = self
            .deps
            .store()
            .apply_for_user(
                ctx.acting_user_id,
                MemoryMutation::Write {
                    agent,
                    kind: parsed.kind,
                    content,
                    state: MemoryState::Tentative,
                    pinned: false,
                    source: MutationSource::Turn(ctx.request_id),
                },
            )
            .await
            .map_err(store_to_tool_err)?;

        debug!(
            patom.session.id = %ctx.session_id,
            patom.agent.id = %agent,
            patom.memory.id = %outcome.memory_id,
            patom.memory.kind = parsed.kind.as_str(),
            "memory_write.ok",
        );

        let out = Output {
            memory_id: outcome.memory_id,
            kind: parsed.kind.as_str(),
            state: MemoryState::Tentative.as_str(),
            note: "Memory written. The new entry will appear under a stable handle on your next session.",
        };
        Ok(serde_json::to_string(&out)?)
    }
}
