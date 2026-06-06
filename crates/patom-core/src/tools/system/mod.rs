//! System tools — first-party capabilities the agent invokes through the
//! tool seam.
//!
//! Five families:
//!
//! * **Communication** — [`SendMessageTool`] (the only delivery mechanism
//!   for messages between participants) and [`GetSessionTool`]
//!   (cross-session read scoped to the caller's DAG). Both consume
//!   [`super::ToolCallContext`].
//! * **Memory** — [`MemoryWriteTool`], [`MemoryUpdateTool`],
//!   [`MemoryForgetTool`], [`MemoryValidateTool`], [`RecallTool`]. The
//!   four journal-writing tools share a per-turn cap via
//!   [`MemoryToolDeps`]; `recall` carries its own.
//! * **Scheduling** — [`ScheduleTaskTool`], [`ListScheduledTasksTool`],
//!   [`CancelScheduledTaskTool`]. Persist a future wake-up; the
//!   `ScheduledTaskScheduler` enqueues a `prompt_requests` row at fire
//!   time so the agent receives a fresh turn.
//! * **Agents** — [`SearchAgentsTool`] for discovery; [`CreateAgentTool`]
//!   for hiring (the recruiter's primary capability).
//! * **Built-in capabilities** — [`WebFetchTool`] and [`WebSearchTool`].
//!
//! Registration lives in the composition root (`src/app.rs`) — there is
//! no `register` helper here. Adding a system tool is one new file in
//! this directory + one `.with(...)` line in `app.rs`. Externally-supplied
//! tools enter through the MCP registry instead of this module.

mod create_agent;
mod get_session;
mod memory;
mod request_user_wire_mcp;
mod scheduling;
mod search_agents;
mod search_tools;
mod send_message;
pub mod todos;
mod web_fetch;
mod web_search;

pub use create_agent::CreateAgentTool;
pub use get_session::GetSessionTool;
pub use memory::{
    MemoryForgetTool, MemoryToolDeps, MemoryUpdateTool, MemoryValidateTool, MemoryWriteTool,
    RecallTool,
};
pub use request_user_wire_mcp::RequestUserWireMcpTool;
pub use scheduling::{CancelScheduledTaskTool, ListScheduledTasksTool, ScheduleTaskTool};
pub use search_agents::SearchAgentsTool;
pub use search_tools::SearchToolsTool;
pub use send_message::SendMessageTool;
pub use todos::{
    PgSessionTodoStore, SessionTodoStore, SharedSessionTodoStore, TodoToolDeps, TodoWriteTool,
};
pub use web_fetch::WebFetchTool;
pub use web_search::WebSearchTool;
