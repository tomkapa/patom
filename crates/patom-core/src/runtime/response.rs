//! Response delivery — trait surface.
//!
//! Two seams: [`ResponseSink`] (worker side — publish chunks) and [`ResponseSource`]
//! (HTTP side — subscribe). The Postgres impl in [`super::pg_response`] is the only
//! backend today.

use std::fmt;
use std::sync::Arc;

use async_trait::async_trait;
use futures::Stream;
use serde::{Deserialize, Serialize};

use crate::agents::AgentId;
use crate::auth::UserId;
use crate::mcp::{McpAuthKind, McpCatalogId};
use crate::provider::{ToolCall, ToolResult};
use crate::threads::ThreadId;

use super::error::ResponseError;
use super::types::{ChunkSeq, FailureReason, PromptRequestId};

/// A single content chunk emitted during a turn.
///
/// `Serialize` / `Deserialize` are the wire format consumed by the SSE handler and
/// the JSONB payload column of `prompt_response_chunks` —
/// `#[serde(tag = "kind", rename_all = "snake_case")]` produces
/// `{"kind":"text","value":"..."}` etc., and [`event_kind`] returns the matching
/// SSE `event:` name. Both come from the same enum so the wire format cannot drift
/// from the type.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ResponseChunk {
    /// Plain assistant text. Always safe to forward to a user-visible UI.
    Text { value: String },
    /// Reasoning (thinking) block. Provider-opaque; surface only to UIs that opt in
    /// since it can be PII-adjacent.
    Reasoning { value: String },
    /// Model issued a tool call. The provider's typed value is reused verbatim so
    /// the wire format cannot drift from the agent's representation.
    ToolCall(ToolCall),
    /// Tool finished. `output` is the bytes the tool returned (already capped by the
    /// agent at `TOOL_RESULT_MAX_BYTES`); `is_error` distinguishes failure from success.
    ToolResult(ToolResult),
    /// An agent's outbound message posted to a thread feed.
    ///
    /// Published by the `send_message` tool on every posted egress row so live
    /// consumers (the Slack stream pump, the web SSE) see the message without
    /// refetching the G2 feed. Non-terminal — the `Done` chunk fires only on
    /// DAG quiescence. `from` lets clients render which agent authored each
    /// message. `to_thread` is the thread the message landed in; delivery
    /// surfaces route by the publishing request's `thread_id` (carried on the
    /// NOTIFY), so `to_thread` is informational and equals that thread.
    AgentMessage {
        from: AgentId,
        to_thread: ThreadId,
        content: String,
        /// The colleague this message is addressed to (the `send_message`
        /// receiver), or `None` for an untagged post. Lets a chat adapter
        /// render a platform `@`-mention of the recipient (e.g. Lark `<at>`).
        /// Optional + skipped-when-`None` so the wire stays backward-compatible.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        to: Option<crate::colleagues::ColleagueId>,
    },
    /// Interactive prompt: the agent is asking the user to wire an MCP
    /// integration from inside the chat thread.
    ///
    /// Emitted by the `request_user_wire_mcp` tool when the recruiter
    /// (or any other agent) decides a still-unwired catalog entry would
    /// fit the role being scoped. The UI renders this as a click-to-wire
    /// card with the recruiter's `reason`, optionally a "Learn more"
    /// link from `homepage_url`, and a "Connect <display_name>" button.
    /// Non-terminal; the recruiter resumes on the user's next turn.
    /// `from` attributes the request to the asking agent.
    WireMcpRequest {
        from: AgentId,
        catalog_id: McpCatalogId,
        display_name: String,
        reason: String,
        auth_kind: McpAuthKind,
        #[serde(skip_serializing_if = "Option::is_none")]
        homepage_url: Option<String>,
    },
    /// Turn completed normally. The full assistant text is included for late
    /// subscribers that don't want to reconstitute from `Text` chunks.
    Done { final_text: String },
    /// Turn failed. `reason` is the failure's `Display` form so SSE clients see
    /// provider/hook detail; `code` is the low-cardinality label
    /// ([`FailureReason::label`]) so clients can branch on the failure kind
    /// (e.g. render a dedicated message for `billing_exceeded`) without parsing
    /// the human text.
    Error { reason: String, code: String },
    /// Slow subscriber overflowed the broadcast buffer; reconnect with `Last-Event-ID`.
    Stalled,
}

impl ResponseChunk {
    #[must_use]
    pub const fn is_terminal(&self) -> bool {
        matches!(self, Self::Done { .. } | Self::Error { .. })
    }

    /// Stable, low-cardinality SSE `event:` name. Mirrors the snake_case wire tag.
    #[must_use]
    pub const fn event_kind(&self) -> &'static str {
        match self {
            Self::Text { .. } => "text",
            Self::Reasoning { .. } => "reasoning",
            Self::ToolCall(_) => "tool_call",
            Self::ToolResult(_) => "tool_result",
            Self::AgentMessage { .. } => "agent_message",
            Self::WireMcpRequest { .. } => "wire_mcp_request",
            Self::Done { .. } => "done",
            Self::Error { .. } => "error",
            Self::Stalled => "stalled",
        }
    }

    /// Approximate byte cost — used to size the persisted log on the storage side.
    /// Tool-call input is sized via `to_string()` length so a JSON object reserves
    /// roughly the right budget.
    #[must_use]
    pub fn weight(&self) -> usize {
        match self {
            Self::Text { value } | Self::Reasoning { value } => value.len(),
            Self::Error { reason, code } => reason.len() + code.len(),
            Self::Done { final_text } => final_text.len(),
            Self::AgentMessage { content, .. } => content.len() + 36, // 36 = uuid str
            Self::WireMcpRequest {
                catalog_id,
                display_name,
                reason,
                homepage_url,
                ..
            } => {
                catalog_id.as_str().len()
                    + display_name.len()
                    + reason.len()
                    + homepage_url.as_deref().map_or(0, str::len)
                    + 36 // from agent id
            }
            Self::ToolCall(c) => {
                c.id.as_str().len() + c.name.as_str().len() + c.input.to_string().len()
            }
            Self::ToolResult(r) => r.call_id.as_str().len() + r.output.len(),
            Self::Stalled => 0,
        }
    }

    /// Build a wire `Error` chunk from a [`FailureReason`]. `reason` carries the
    /// full `Display` form so SSE clients see provider/hook detail; `code`
    /// carries the low-cardinality [`FailureReason::label`] so clients can
    /// branch on the failure kind.
    #[must_use]
    pub fn from_failure(reason: &FailureReason) -> Self {
        Self::Error {
            reason: reason.to_string(),
            code: reason.label().to_owned(),
        }
    }
}

/// A chunk paired with its monotonic sequence number.
#[derive(Debug, Clone)]
pub struct ResponseChunkEnvelope {
    pub seq: ChunkSeq,
    pub chunk: ResponseChunk,
}

/// What an SSE stream observer sees. Wraps the broadcast lag behaviour.
#[derive(Debug, Clone)]
pub enum StreamEvent {
    Chunk(ResponseChunkEnvelope),
    /// Buffer exhausted between sends — the next attached subscriber must reconnect.
    Stalled,
}

#[async_trait]
pub trait ResponseSink: fmt::Debug + Send + Sync {
    async fn publish(
        &self,
        request_id: PromptRequestId,
        chunk: ResponseChunk,
    ) -> Result<ChunkSeq, ResponseError>;
    async fn close(&self, request_id: PromptRequestId) -> Result<(), ResponseError>;

    /// Tenant-scoped variant of [`Self::publish`]. Opens
    /// `begin_as_user(acting_user_id)` so the `prompt_response_streams`
    /// / `prompt_response_chunks` INSERTs are RLS-checked against the
    /// acting principal. Worker / tool callers source `acting_user_id`
    /// from the claimed session's `created_by_user_id`; HTTP and
    /// scheduler paths keep the existing privileged entry point.
    async fn publish_for_user(
        &self,
        acting_user_id: UserId,
        request_id: PromptRequestId,
        chunk: ResponseChunk,
    ) -> Result<ChunkSeq, ResponseError>;

    /// Tenant-scoped variant of [`Self::close`].
    async fn close_for_user(
        &self,
        acting_user_id: UserId,
        request_id: PromptRequestId,
    ) -> Result<(), ResponseError>;
}

#[async_trait]
pub trait ResponseSource: fmt::Debug + Send + Sync {
    /// Subscribe to a request's stream from `since` (exclusive). Replays any persisted
    /// chunks then attaches to the live broadcast. If the request is unknown, returns
    /// [`ResponseError::NotFound`].
    async fn subscribe(
        &self,
        request_id: PromptRequestId,
        since: Option<ChunkSeq>,
    ) -> Result<RequestStream, ResponseError>;
}

/// A boxed stream the SSE handler iterates. `Send` so it can move across awaits.
pub type RequestStream =
    std::pin::Pin<Box<dyn Stream<Item = Result<StreamEvent, ResponseError>> + Send>>;

/// Reference-counted publish-side handle held by workers.
pub type SharedResponseSink = Arc<dyn ResponseSink>;

/// Reference-counted subscribe-side handle held by HTTP routes.
pub type SharedResponseSource = Arc<dyn ResponseSource>;
