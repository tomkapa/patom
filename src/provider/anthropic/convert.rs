//! Bidirectional conversion between provider-agnostic chat types and `claudius` types.
//!
//! Kept in its own module so each direction sits in one place and the cognitive load of
//! adding a new content variant is bounded.

use claudius::{
    ContentBlock, MessageParam, MessageRole, Model, StopReason as ClaudiusStop, TextBlock,
    ToolParam, ToolResultBlock, ToolUnionParam, ToolUseBlock,
};

use crate::provider::chat::{
    AssistantContent, ChatMessage, StopReason, ToolCall, ToolCallId, ToolSpec, UserContent,
};
use crate::types::ToolName;

/// Map a provider-agnostic message into a claudius `MessageParam`. Returns `None` for a
/// message that would serialize to an empty block list — Anthropic rejects an
/// empty-content message, so the caller drops it from the replayed history.
pub(super) fn message_to_param(msg: ChatMessage) -> Option<MessageParam> {
    match msg {
        ChatMessage::User(content) => {
            let blocks = content.into_iter().map(user_content_to_block).collect();
            Some(MessageParam::new_with_blocks(blocks, MessageRole::User))
        }
        ChatMessage::Assistant(content) => {
            let blocks: Vec<ContentBlock> = content
                .into_iter()
                .filter_map(assistant_content_to_block)
                .collect();
            // Reasoning blocks drop out (no Anthropic signature to replay), so a
            // reasoning-only turn leaves an empty block list. Emitting an empty-content
            // assistant message is rejected and wedges the session on every replay, so
            // omit it from the wire entirely. The block stays persisted for audit.
            (!blocks.is_empty())
                .then(|| MessageParam::new_with_blocks(blocks, MessageRole::Assistant))
        }
    }
}

fn user_content_to_block(c: UserContent) -> ContentBlock {
    match c {
        UserContent::Text(t) => ContentBlock::Text(TextBlock::new(t)),
        UserContent::ToolResult(r) => {
            let mut block =
                ToolResultBlock::new(r.call_id.as_str().to_string()).with_string_content(r.output);
            // Anthropic only wants `is_error` set when true — sending `is_error: false`
            // is technically valid but adds noise on the wire and in caching keys.
            if r.is_error {
                block = block.with_error(true);
            }
            ContentBlock::ToolResult(block)
        }
    }
}

/// Reasoning blocks are observability-only on our side; we drop them when re-serializing
/// history back to the provider. Replaying them requires Anthropic-specific signature
/// preservation that does not generalise to other providers — when we add streaming
/// thinking proper, this is the seam to revisit.
fn assistant_content_to_block(c: AssistantContent) -> Option<ContentBlock> {
    match c {
        AssistantContent::Text(t) => Some(ContentBlock::Text(TextBlock::new(t))),
        AssistantContent::ToolCall(call) => Some(ContentBlock::ToolUse(ToolUseBlock::new(
            call.id.as_str().to_string(),
            call.name.as_str().to_string(),
            call.input,
        ))),
        AssistantContent::Reasoning(_) => None,
    }
}

/// Map a tool spec to claudius's tool-union representation.
pub(super) fn tool_spec_to_param(spec: &ToolSpec) -> ToolUnionParam {
    let param = ToolParam::new(spec.name.as_str().to_string(), (*spec.input_schema).clone())
        .with_description(spec.description.to_string());
    ToolUnionParam::CustomTool(param)
}

/// Parse a model identifier. `claudius::Model::from_str` is infallible (it falls back to
/// `Custom`), so this never errors today — kept fallible for forward compatibility if the
/// upstream contract tightens.
pub(super) fn parse_model(raw: &str) -> Model {
    raw.parse::<Model>()
        .unwrap_or_else(|()| Model::Custom(raw.to_string()))
}

/// Lift a claudius response block into the provider-agnostic shape. Returns `None` for
/// content variants we deliberately don't surface yet (server-side tool use, web search
/// results, redacted thinking) — when the agent grows to use them, add the mapping here.
pub(super) fn block_to_assistant(block: ContentBlock) -> Option<AssistantContent> {
    match block {
        ContentBlock::Text(t) => Some(AssistantContent::Text(t.text)),
        ContentBlock::Thinking(t) => Some(AssistantContent::Reasoning(t.thinking)),
        ContentBlock::ToolUse(t) => {
            // A name or id we cannot parse means the upstream sent something we wouldn't
            // have registered — drop it so the agent loop terminates cleanly rather than
            // looping on an unknown tool.
            let name = ToolName::try_from(t.name.as_str()).ok()?;
            let id = ToolCallId::try_from(t.id.as_str()).ok()?;
            Some(AssistantContent::ToolCall(ToolCall {
                id,
                name,
                input: t.input,
            }))
        }
        _ => None,
    }
}

pub(super) fn map_stop_reason(stop: Option<ClaudiusStop>) -> StopReason {
    match stop {
        Some(ClaudiusStop::EndTurn) | None => StopReason::EndTurn,
        Some(ClaudiusStop::ToolUse) => StopReason::ToolUse,
        Some(ClaudiusStop::MaxTokens) => StopReason::MaxTokens,
        Some(other) => StopReason::Other(other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reasoning_only_assistant_message_is_dropped() {
        // Reasoning blocks have no Anthropic signature to replay, so a reasoning-only
        // turn would serialize to an empty-content message — which Anthropic rejects and
        // which wedges the session on every replay. It must be omitted from the wire.
        let msg = ChatMessage::Assistant(vec![AssistantContent::Reasoning("secret".into())]);
        assert!(message_to_param(msg).is_none());
    }

    #[test]
    fn assistant_message_with_text_is_kept() {
        let msg = ChatMessage::Assistant(vec![AssistantContent::Text("hello".into())]);
        assert!(message_to_param(msg).is_some());
    }
}
