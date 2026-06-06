//! Scheduling tools — agent-driven scheduling of future wake-up turns.
//!
//! A scheduled task fires through the standard `Normal` queue path:
//! the `ScheduledTaskScheduler` enqueues a `prompt_requests` row with
//! `sender = Participant::Human`, `parent_session = None`, addressed to
//! the owning agent. The fired turn looks indistinguishable from a
//! human-initiated `POST /prompts` to the worker pool.
//!
//! Authorization model:
//!
//! 1. The caller must be an agent (`ToolCallContext.viewer.is_agent()`).
//!    Humans don't run tool calls; this is defence in depth.
//! 2. The caller's `agent_id` becomes the task's `owner_agent_id`.
//!    `list_scheduled_tasks` filters by it; `cancel_scheduled_task`
//!    refuses cross-owner mutation with `OwnerMismatch` → `InvalidInput`.
//!
//! Three tools, each in its own file:
//!
//! - [`ScheduleTaskTool`] (`schedule_task`) — register a future wake-up.
//! - [`ListScheduledTasksTool`] (`list_scheduled_tasks`) — list the
//!   caller's active tasks.
//! - [`CancelScheduledTaskTool`] (`cancel_scheduled_task`) — cancel by
//!   id, scoped to the caller.

mod cancel_scheduled_task;
mod list_scheduled_tasks;
mod schedule_task;

pub use cancel_scheduled_task::CancelScheduledTaskTool;
pub use list_scheduled_tasks::ListScheduledTasksTool;
pub use schedule_task::ScheduleTaskTool;

// The `<scheduling>` supplement body lives in `src/prompts/internal.toml`
// under the `scheduling_supplement` key and is joined onto the base
// `Normal` `<core>` body at startup by `Prompts::load`. The tool docs
// stay in this module (close to the impls); the model-facing copy lives
// next to the other prompts so a translation pass touches one TOML.
