//! `get_session` — read-only cross-session lookup, scoped to the caller's DAG.
//!
//! Agents fork sub-conversations through `send_message`. Most of the time a
//! child agent only needs its parent (the prompt is auto-loaded — see
//! `Agent::with_parent_context`); but anything beyond one hop is the
//! agent's responsibility, and `get_session` is the read seam.
//!
//! Authorization (per the multi-agent design):
//!
//! 1. The target session's `root_request_id` must equal the caller's
//!    current DAG root. Anything outside the caller's causal subgraph is
//!    refused — agents from another conversation tree cannot peek.
//! 2. The caller must be a participant of the target session — sessions
//!    are 2-party, so this is "your direct conversations only".
//!
//! Pagination uses `(limit, before_seq)`: default `limit = 50`, hard cap
//! [`crate::runtime::MAX_GET_SESSION_LIMIT`]. The output `next_before_seq`
//! is the lowest `seq` returned, ready to feed into the next call when the
//! caller wants to keep walking backward.

use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tracing::warn;

use crate::provider::ChatMessage;
use crate::runtime::MAX_GET_SESSION_LIMIT;
use crate::session::{SessionError, SessionId, SharedSessionStore};
use crate::types::ToolName;

use super::super::traits::{Tool, ToolCallContext, ToolError};

const TOOL_NAME: &str = "get_session";

const TOOL_DESCRIPTION: &str = "Fetch the message history of a session you are \
    a participant of, scoped to your current task's DAG.\n\
    \n\
    USE THIS ONLY for cross-session lookup beyond the immediate parent the \
    system already auto-loads — e.g. inspecting a sibling sub-conversation \
    you spawned earlier. The current session's own history is already in \
    the messages you see; you do not need this tool to read it.\n\
    \n\
    DO NOT USE THIS to wait for a reply on a session you just messaged. \
    `send_message` is asynchronous: the receiver runs in a separate worker \
    turn, and their reply will arrive in YOUR NEXT TURN automatically. \
    Polling here returns the same stale snapshot every time and just wastes \
    turn budget. Send, end the turn, and the next turn will show their \
    reply.\n\
    \n\
    Arguments: `session_id` (the session to read), optional `limit` (1-200, \
    default 50), optional `before_seq` (cursor; pass the `next_before_seq` \
    from a previous call to walk backward). Refuses sessions outside your \
    DAG or sessions you are not a participant of.";

const DEFAULT_LIMIT: u32 = 50;

#[derive(Debug, Deserialize)]
struct GetSessionInput {
    session_id: SessionId,
    #[serde(default)]
    limit: Option<u32>,
    #[serde(default)]
    before_seq: Option<i64>,
}

#[derive(Debug, Serialize)]
struct GetSessionOutput {
    session_id: SessionId,
    messages: Vec<RenderedMessage>,
    /// Lowest seq returned. Pass back as `before_seq` in a subsequent call
    /// to fetch older messages. `None` when fewer than `limit` rows came
    /// back — the caller has reached the start.
    next_before_seq: Option<i64>,
}

#[derive(Debug, Serialize)]
struct RenderedMessage {
    seq: i64,
    /// Viewer-mapped role: "assistant" when the caller authored the row,
    /// "user" otherwise (including system rows).
    role: &'static str,
    /// Concatenated text content. Tool-call/tool-result blocks are rendered
    /// as inline `[tool ...]` markers — the lookup is informational, the
    /// caller doesn't replay tool calls.
    text: String,
}

pub struct GetSessionTool {
    name: ToolName,
    description: &'static str,
    input_schema: Arc<Value>,
    sessions: SharedSessionStore,
}

impl std::fmt::Debug for GetSessionTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GetSessionTool").finish_non_exhaustive()
    }
}

impl GetSessionTool {
    #[must_use]
    pub fn new(sessions: SharedSessionStore) -> Self {
        let name = ToolName::try_from(TOOL_NAME).expect("invariant: get_session is a valid name");
        let input_schema = Arc::new(json!({
            "type": "object",
            "required": ["session_id"],
            "properties": {
                "session_id": { "type": "string", "format": "uuid" },
                "limit": {
                    "type": ["integer", "null"],
                    "minimum": 1,
                    "maximum": MAX_GET_SESSION_LIMIT,
                },
                "before_seq": { "type": ["integer", "null"], "minimum": 0 }
            },
            "additionalProperties": false,
        }));
        Self {
            name,
            description: TOOL_DESCRIPTION,
            input_schema,
            sessions,
        }
    }

    async fn handle(
        &self,
        input: GetSessionInput,
        ctx: &ToolCallContext,
    ) -> Result<GetSessionOutput, ToolError> {
        let limit = clamp_limit(input.limit.unwrap_or(DEFAULT_LIMIT))?;
        self.authorize(input.session_id, ctx).await?;

        let rows = self
            .sessions
            .snapshot_window(input.session_id, ctx.viewer, limit, input.before_seq)
            .await
            .map_err(|e| ToolError::Backend(format!("get_session: snapshot failed: {e}")))?;

        // Comparing as i64 sidesteps the as-usize cast clippy refuses; the
        // window is bounded by `MAX_GET_SESSION_LIMIT` so neither side can
        // exceed `i64::MAX`.
        let returned = i64::try_from(rows.len()).unwrap_or(i64::MAX);
        let next_before_seq = if returned == i64::from(limit) {
            rows.first().map(|(seq, _)| *seq)
        } else {
            None
        };

        let messages = rows
            .into_iter()
            .map(|(seq, m)| RenderedMessage {
                seq,
                role: viewer_role(&m),
                text: render_message_text(&m),
            })
            .collect();

        Ok(GetSessionOutput {
            session_id: input.session_id,
            messages,
            next_before_seq,
        })
    }

    /// Enforce the two scoping rules from the spec — same DAG, caller is a
    /// participant — before reading any rows. Both lookups are O(1) on
    /// indexed columns, so this is cheap.
    async fn authorize(&self, target: SessionId, ctx: &ToolCallContext) -> Result<(), ToolError> {
        let root = match self.sessions.root_request_id(target).await {
            Ok(r) => r,
            Err(SessionError::NotFound(_)) => {
                warn!(patom.session.id = %target, "get_session.target_not_found");
                return Err(ToolError::InvalidInput(format!(
                    "get_session: session {target} not found"
                )));
            }
            Err(e) => {
                warn!(error = %e, patom.session.id = %target, "get_session.target_lookup");
                return Err(ToolError::Backend(format!(
                    "get_session: session lookup: {e}"
                )));
            }
        };
        if root != ctx.root_request_id {
            return Err(ToolError::InvalidInput(
                "get_session: session is outside your task's DAG".into(),
            ));
        }
        let (a, b) =
            self.sessions.participants(target).await.map_err(|e| {
                ToolError::Backend(format!("get_session: participants lookup: {e}"))
            })?;
        if a != ctx.viewer && b != ctx.viewer {
            return Err(ToolError::InvalidInput(
                "get_session: you are not a participant of this session".into(),
            ));
        }
        Ok(())
    }
}

#[async_trait]
impl Tool for GetSessionTool {
    fn name(&self) -> &ToolName {
        &self.name
    }

    fn description(&self) -> &str {
        self.description
    }

    fn input_schema(&self) -> Arc<Value> {
        self.input_schema.clone()
    }

    fn concurrency_safe(&self) -> bool {
        true
    }

    async fn execute(&self, input: Value, ctx: &ToolCallContext) -> Result<String, ToolError> {
        let parsed: GetSessionInput = serde_json::from_value(input)?;
        let out = self.handle(parsed, ctx).await?;
        Ok(serde_json::to_string(&out)?)
    }
}

fn clamp_limit(requested: u32) -> Result<u32, ToolError> {
    if requested == 0 {
        return Err(ToolError::InvalidInput(
            "get_session: limit must be >= 1".into(),
        ));
    }
    if requested > MAX_GET_SESSION_LIMIT {
        return Err(ToolError::InvalidInput(format!(
            "get_session: limit exceeds cap ({MAX_GET_SESSION_LIMIT})"
        )));
    }
    Ok(requested)
}

fn viewer_role(m: &ChatMessage) -> &'static str {
    match m {
        ChatMessage::Assistant(_) => "assistant",
        ChatMessage::User(_) => "user",
    }
}

fn render_message_text(m: &ChatMessage) -> String {
    use crate::provider::{AssistantContent, UserContent};
    match m {
        ChatMessage::User(blocks) => blocks
            .iter()
            .map(|b| match b {
                UserContent::Text(t) => t.clone(),
                UserContent::ToolResult(r) => {
                    format!("[tool-result {}: {}]", r.call_id.as_str(), r.output)
                }
            })
            .collect::<Vec<_>>()
            .join("\n"),
        ChatMessage::Assistant(blocks) => blocks
            .iter()
            .map(|b| match b {
                AssistantContent::Text(t) | AssistantContent::Reasoning(t) => t.clone(),
                AssistantContent::ToolCall(c) => {
                    format!("[tool-call {}({})]", c.name.as_str(), c.input)
                }
            })
            .collect::<Vec<_>>()
            .join("\n"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clamp_limit_rejects_zero_and_over_cap() {
        assert!(clamp_limit(0).is_err());
        assert!(clamp_limit(MAX_GET_SESSION_LIMIT + 1).is_err());
    }

    #[test]
    fn clamp_limit_passes_through_valid() {
        assert_eq!(clamp_limit(1).expect("ok"), 1);
        assert_eq!(
            clamp_limit(MAX_GET_SESSION_LIMIT).expect("ok"),
            MAX_GET_SESSION_LIMIT
        );
    }

    #[test]
    fn render_message_text_handles_all_block_kinds() {
        use crate::provider::{
            AssistantContent, ChatMessage, ToolCall, ToolCallId, ToolResult, UserContent,
        };
        use crate::types::ToolName;
        use serde_json::json;

        let user = ChatMessage::User(vec![
            UserContent::Text("hi".into()),
            UserContent::ToolResult(ToolResult {
                call_id: ToolCallId::try_from("call-1").expect("ok"),
                output: "done".into(),
                is_error: false,
            }),
        ]);
        let rendered = render_message_text(&user);
        assert!(rendered.contains("hi"));
        assert!(rendered.contains("[tool-result"));

        let assistant = ChatMessage::Assistant(vec![
            AssistantContent::Text("ok".into()),
            AssistantContent::ToolCall(ToolCall {
                id: ToolCallId::try_from("call-2").expect("ok"),
                name: ToolName::try_from("counter").expect("ok"),
                input: json!({"n": 1}),
            }),
        ]);
        let rendered = render_message_text(&assistant);
        assert!(rendered.contains("ok"));
        assert!(rendered.contains("[tool-call"));
    }
}
