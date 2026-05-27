//! Per-turn DEBUG logging of assistant blocks and tool results.
//!
//! Mirrors what the SSE observer streams. Demoted off the default INFO
//! console — investigate with `RUST_LOG=patom_rs=debug`.

use tracing::{debug, info};

use crate::observability::log::preview;
use crate::provider::{AssistantContent, ToolResult};

pub(super) fn assistant_block(turn: u32, block: &AssistantContent) {
    match block {
        AssistantContent::Text(t) => debug!(
            turn,
            kind = "text",
            preview = %preview(t),
            "agent.turn.assistant",
        ),
        AssistantContent::Reasoning(t) => debug!(
            turn,
            kind = "reasoning",
            preview = %preview(t),
            "agent.turn.assistant",
        ),
        AssistantContent::ToolCall(call) => debug!(
            turn,
            kind = "tool_call",
            patom.tool = %call.name,
            patom.tool.call_id = call.id.as_str(),
            input.preview = %preview(&call.input.to_string()),
            "agent.turn.assistant",
        ),
    }
}

/// Successes are DEBUG; errors stay at INFO so a `grep tool_result.err`
/// triages without enabling debug logs.
pub(super) fn tool_result(turn: u32, result: &ToolResult) {
    if result.is_error {
        info!(
            turn,
            patom.tool.call_id = result.call_id.as_str(),
            output.preview = %preview(&result.output),
            "agent.turn.tool_result.err",
        );
    } else {
        debug!(
            turn,
            patom.tool.call_id = result.call_id.as_str(),
            output.preview = %preview(&result.output),
            "agent.turn.tool_result.ok",
        );
    }
}
