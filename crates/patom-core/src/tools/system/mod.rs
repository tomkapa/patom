//! System tools — first-party capabilities the agent invokes through the
//! tool seam.
//!
//! Five families:
//!
//! * **Communication** — [`SendMessageTool`] (the only delivery mechanism
//!   for messages between participants). Consumes [`super::ToolCallContext`].
//! * **Cross-thread read** — [`ReadChannelTool`] (#199), the one tool that lets
//!   an agent see outside its own thread: the recent history of a channel it is
//!   a **member** of, for summarising on a cadence or on demand. The agent still
//!   reads its full *current* thread at run time; this adds a bounded,
//!   membership-gated read of *other* channels it belongs to. The membership
//!   gate (`ThreadStore::colleague_in_channel`, the same one `send_message`
//!   uses) is the entire safety boundary — there is no way to read a channel the
//!   agent is not a member of.
//! * **Memory** — [`MemoryWriteTool`], [`MemoryUpdateTool`],
//!   [`MemoryForgetTool`], [`MemoryValidateTool`], [`RecallTool`]. The
//!   four journal-writing tools share a per-turn cap via
//!   [`MemoryToolDeps`]; `recall` carries its own.
//! * **Scheduling** — [`ScheduleTaskTool`], [`ListScheduledTasksTool`],
//!   [`CancelScheduledTaskTool`]. Persist a future wake-up; the
//!   `ScheduledTaskScheduler` enqueues a `prompt_requests` row at fire
//!   time so the agent receives a fresh turn.
//! * **Colleagues** — [`SearchColleagueTool`] for discovery (agents + profiled
//!   humans, one ranked list); [`CreateAgentTool`] for hiring (the recruiter's
//!   primary capability); [`ProfileWriteTool`] to record a colleague's shared
//!   role/expertise/preferences on the org board.
//! * **Built-in capabilities** — [`WebFetchTool`] and [`WebSearchTool`].
//!
//! Registration lives in the composition root (`src/app.rs`) — there is
//! no `register` helper here. Adding a system tool is one new file in
//! this directory + one `.with(...)` line in `app.rs`. Externally-supplied
//! tools enter through the MCP registry instead of this module.

mod create_agent;
mod memory;
mod profile_write;
mod read_channel;
mod request_user_wire_mcp;
mod scheduling;
mod search_colleague;
mod search_tools;
mod send_message;
pub mod todos;
mod web_fetch;
mod web_search;

pub use create_agent::CreateAgentTool;
pub use memory::{
    MemoryForgetTool, MemoryToolDeps, MemoryUpdateTool, MemoryValidateTool, MemoryWriteTool,
    RecallTool,
};
pub use profile_write::ProfileWriteTool;
pub use read_channel::ReadChannelTool;
pub use request_user_wire_mcp::RequestUserWireMcpTool;
pub use scheduling::{CancelScheduledTaskTool, ListScheduledTasksTool, ScheduleTaskTool};
pub use search_colleague::SearchColleagueTool;
pub use search_tools::SearchToolsTool;
pub use send_message::SendMessageTool;
pub use todos::{
    PgSessionTodoStore, SessionTodoStore, SharedSessionTodoStore, TodoToolDeps, TodoWriteTool,
};
pub use web_fetch::WebFetchTool;
pub use web_search::WebSearchTool;
