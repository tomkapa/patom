//! Agent-driven scheduled tasks.
//!
//! An agent calls the `schedule_task` system tool to register a future
//! wake-up. At each fire time the [`ScheduledTaskScheduler`] enqueues a
//! `prompt_requests` row addressed to the same agent, with the stored
//! prompt as content and `Participant::Human` as sender. The worker pool
//! dispatches the resulting turn through the existing `Normal` path —
//! no new queue kind, no new worker dispatch.
//!
//! Two-variant schedule space (`ScheduleSpec::{Once, Recurring}`) covers
//! the personal-assistant scheduling surface:
//!
//! * `Once` — one-shot at a specific UTC instant.
//! * `Recurring` — fires on a set of weekdays at a `(time-of-day, tz)` in
//!   IANA-timezone-aware local clock. Subsumes "every day", "every
//!   workday", "every Monday", "weekends only", "Mon/Wed/Fri".
//!
//! Calendar-anchored patterns (nth-weekday-of-month, "1st of every
//! month") are deliberately out of scope — agents that need them
//! self-reschedule a `Once` at the end of each fire.
//!
//! Composition: tools live in [`crate::tools::system`]; the scheduler
//! background task and the `ScheduledTaskStore` trait live here. The
//! [`ScheduledTask`] scaffold this module exports is reused by the
//! memory subsystem's [`crate::memory::ReflectionScheduler`] and
//! [`crate::memory::LibrarianScheduler`].

mod error;
mod limits;
mod pg_store;
pub(crate) mod scheduled_task;
mod scheduler;
mod store;
mod types;

pub use error::ScheduledTaskError;
pub use limits::{
    DEFAULT_RECURRING_TIMEZONE, MAX_ONESHOT_HORIZON_DAYS, MAX_SCHEDULED_TASKS_PER_AGENT,
    SCHEDULED_TASK_BATCH_LIMIT, SCHEDULED_TASK_NAME_MAX_LEN, SCHEDULED_TASK_POLL_SECS,
};
pub use pg_store::PgScheduledTaskStore;
pub(crate) use scheduled_task::ScheduledTask;
pub use scheduler::ScheduledTaskScheduler;
pub use store::{
    NewScheduledTask, ScheduledTaskStore, ScheduledTaskUpdate, SharedScheduledTaskStore,
};
pub use types::{
    DefaultTimezone, ScheduleSpec, ScheduledPrompt, ScheduledTaskId, ScheduledTaskName,
    ScheduledTaskRecord, ScheduledTaskState, TimeOfDay, Timezone, Weekday, Weekdays,
};
