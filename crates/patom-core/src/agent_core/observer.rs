//! Streaming hook for the agent loop.
//!
//! Hooks (`HookChain`) are control-plane: they Continue / Deny. A `TurnObserver` is
//! observation-only and exists to let the SSE pipeline emit chunks at every boundary
//! the agent crosses inside one call to [`Agent::reply`](super::Agent::reply) — text,
//! reasoning, tool calls as they appear; tool results as they finish — instead of
//! waiting for the whole loop to terminate.
//!
//! Implementations must not block: they are awaited inline on the agent's hot path.

use std::sync::Arc;

use async_trait::async_trait;

use crate::provider::{AssistantContent, ToolResult};

#[async_trait]
pub trait TurnObserver: Send + Sync {
    /// One call per `AssistantContent` block returned by the provider — text,
    /// reasoning (thinking), or tool call.
    async fn on_assistant(&self, content: &AssistantContent);

    /// One call per tool result, fired the moment the tool returns (success or error).
    async fn on_tool_result(&self, result: &ToolResult);
}

/// Reference-counted handle so the agent can hold an observer without taking a
/// generic parameter.
pub type SharedTurnObserver = Arc<dyn TurnObserver>;

/// No-op observer.
///
/// `Agent::reply` takes `Option<SharedTurnObserver>` — pass `None` for a
/// non-streaming caller. This impl is here for tests that want a sentinel they can
/// assert on without writing a custom impl.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoopObserver;

#[async_trait]
impl TurnObserver for NoopObserver {
    async fn on_assistant(&self, _content: &AssistantContent) {}
    async fn on_tool_result(&self, _result: &ToolResult) {}
}
