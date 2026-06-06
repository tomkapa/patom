//! Structured audit log for every tool invocation the agent dispatcher executes.
//!
//! Today MCP tool calls survive only as JSONB inside `session_messages.body`,
//! so dashboards that want "calls per MCP connection" or "calls per agent"
//! have to scan every message row. The [`ToolCallStore`] trait is the seam
//! the dispatcher writes through after each tool result, with the Postgres
//! implementation in [`super::pg_recorder`].
//!
//! Generic by construction: `mcp_server_id` is `Option<…>` so future
//! non-MCP call types can reuse the same store/table by leaving it `None`.
//! MCP is the first writer; not the last.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use thiserror::Error;

use crate::agents::AgentId;
use crate::auth::OrgId;
use crate::mcp::McpServerId;
use crate::runtime::PromptRequestId;
use crate::session::SessionId;
use crate::types::ToolName;

crate::uuid_newtype! {
    /// Opaque identifier for one row in `tool_calls`.
    ///
    /// Minted by the recorder, not by the model — the agent loop already
    /// carries provider-side `ToolCallId`s (string-typed, scoped to the
    /// LLM conversation). This is a separate, database-scoped id so the
    /// audit row remains addressable even if the provider id is missing
    /// or non-unique (e.g. a future re-run of the same tool call).
    pub ToolCallRowId
}

/// One audit row to write. Built by the dispatcher in
/// `agent_core::turn::run_tools` immediately after a tool returns.
#[derive(Debug, Clone)]
pub struct ToolCallRow {
    pub id: ToolCallRowId,
    pub org_id: OrgId,
    pub session_id: SessionId,
    pub request_id: PromptRequestId,
    pub agent_id: AgentId,
    pub mcp_server_id: Option<McpServerId>,
    pub tool_name: ToolName,
    pub started_at: DateTime<Utc>,
    /// Wall-clock duration of `Tool::execute`, measured by the agent's
    /// `SharedClock` around the await. Converted to `i32` milliseconds at
    /// insert time, saturating at
    /// [`super::limits::MAX_TOOL_CALL_DURATION_MS`] (a `tracing::warn!`
    /// fires on saturation — clipping is rare in practice since the
    /// agent's `tool_timeout` runs well below the cap).
    pub duration: Duration,
    pub is_error: bool,
    /// Short textual reason set only when `is_error == true`. The recorder
    /// asserts the invariant before insert and the migration-27 CHECK
    /// (`is_error OR error_message IS NULL`) catches any breach that
    /// reaches the database. Already clipped to
    /// [`super::limits::MAX_TOOL_CALL_ERROR_MESSAGE_BYTES`] by the caller.
    pub error_message: Option<String>,
}

#[derive(Debug, Error)]
pub enum ToolCallStoreError {
    #[error(transparent)]
    Db(#[from] sqlx::Error),

    #[error("duration {ms}ms exceeds saturation cap {cap}ms")]
    DurationOverflow { ms: u128, cap: i32 },
}

/// Persistence seam for tool-call audit rows.
///
/// Implementations must be cheap to clone (they live behind `Arc`) and
/// must not block: `record` is awaited inline on the agent's hot path
/// after each tool result.
#[async_trait]
pub trait ToolCallStore: std::fmt::Debug + Send + Sync {
    async fn record(&self, row: ToolCallRow) -> Result<(), ToolCallStoreError>;
}

/// Reference-counted handle plumbed through the agent builder.
pub type SharedToolCallStore = Arc<dyn ToolCallStore>;
