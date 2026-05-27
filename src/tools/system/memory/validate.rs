//! `memory_validate` — records an independent-signal validation against
//! an existing memory (doc/memory.md §1.7, §1.10).
//!
//! Distinct from the three mutation tools: validation does NOT change a
//! memory's content. It records that the agent gathered an independent
//! signal — `web_search`, `web_fetch`, a peer reply, a human reached via
//! `send_message`, *or* an explicit affirmation from the user in the
//! current turn — that supports the memory. All five route through the
//! `ValidationSource::ExternalConfirmation` channel; the operator audit
//! reads the `evidence` text to distinguish which signal fired. The
//! journal logs both a `validation_events` row carrying the agent's
//! free-text evidence and a wrapping `memory_events` Update event for
//! any state promotion, so replay reconstructs the lifecycle faithfully.
//!
//! Sharing the per-turn mutation counter is deliberate: validation writes
//! to the journal (twice, per call) and advances state, so capping it
//! alongside `memory_write` / `memory_update` / `memory_forget` prevents a
//! runaway turn from spamming validations to inflate the lifecycle clock.
//!
//! Pinned-row protection still applies — agent-driven validation against
//! a pinned memory returns `PinnedImmutable`, the same outcome as a
//! mutation attempt. Pinned memories already carry operator authority;
//! external confirmation on top is redundant.

use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tracing::debug;

use crate::memory::{
    MemoryEvidence, MemoryHandle, MemoryId, VALIDATION_EVIDENCE_MAX_BYTES, ValidationOrigin,
};
use crate::tools::{Tool, ToolCallContext, ToolError};
use crate::types::ToolName;

use super::{
    MemoryToolDeps, check_cap, expect_agent, parse_to_tool_err, resolve_handle, store_to_tool_err,
};

const TOOL_NAME: &str = "memory_validate";

const TOOL_DESCRIPTION: &str = "Record that an independent signal has confirmed an existing memory. Two \
     valid triggers, both external to your own reasoning:\n\
     1. A `recall` surfaces memory `M-NN` and a follow-up `web_search`, \
        `web_fetch`, or `send_message` returns content that supports it — \
        \"I recalled M-12 (we deploy Mondays), web_search confirmed the \
        deployment cadence\".\n\
     2. The user explicitly affirmed an existing memory in the current turn — \
        \"yes, that's right\", \"correct, M-7 still holds\", \"confirmed, I \
        do prefer Python\".\n\
     \n\
     This does NOT change the memory's content; it advances the validation \
     clock so the memory promotes (Tentative → Held → Validated). Do NOT call \
     this on the strength of your own reasoning alone — that is rubber-stamping \
     and defeats the anti-drift design. The `evidence` you cite (the search \
     snippet, the peer's reply, or the user's exact affirming words) lands \
     verbatim in the journal for operator audit.\n\
     \n\
     Arguments: `handle` (the `M-NN` form from the `## Memory` section), \
     `evidence` (one or two sentences citing the external signal, max 1024 bytes).";

#[derive(Debug, Deserialize)]
struct Input {
    handle: String,
    evidence: String,
}

#[derive(Debug, Serialize)]
struct Output {
    memory_id: MemoryId,
    handle: String,
    state: &'static str,
}

pub struct MemoryValidateTool {
    name: ToolName,
    description: &'static str,
    input_schema: Arc<Value>,
    deps: MemoryToolDeps,
}

impl std::fmt::Debug for MemoryValidateTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MemoryValidateTool").finish_non_exhaustive()
    }
}

impl MemoryValidateTool {
    #[must_use]
    pub fn new(deps: MemoryToolDeps) -> Self {
        let name = ToolName::try_from(TOOL_NAME).expect("valid tool name");
        let input_schema = Arc::new(json!({
            "type": "object",
            "required": ["handle", "evidence"],
            "properties": {
                "handle": { "type": "string", "pattern": "^M-[0-9]+$" },
                "evidence": {
                    "type": "string",
                    "minLength": 1,
                    "maxLength": VALIDATION_EVIDENCE_MAX_BYTES,
                }
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
impl Tool for MemoryValidateTool {
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
        let evidence = MemoryEvidence::try_from(parsed.evidence).map_err(parse_to_tool_err)?;

        let row = self
            .deps
            .store()
            .record_validation_for_user(
                ctx.acting_user_id,
                agent,
                memory_id,
                ValidationOrigin::Agent(ctx.request_id),
                Some(evidence.as_str()),
            )
            .await
            .map_err(store_to_tool_err)?;

        debug!(
            patom.session.id = %ctx.session_id,
            patom.agent.id = %agent,
            patom.memory.id = %row.id,
            patom.memory.state = row.state.as_str(),
            "memory_validate.ok",
        );

        let out = Output {
            memory_id: row.id,
            handle: handle.to_string(),
            state: row.state.as_str(),
        };
        Ok(serde_json::to_string(&out)?)
    }
}
