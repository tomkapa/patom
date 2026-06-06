//! Scheduling subsystem caps. CLAUDE.md §5 — every value is documented
//! with *why this number*. Caps that gate the tool boundary, the store,
//! and the background scheduler all share one source of truth here.

use std::time::Duration;

use chrono_tz::Tz;

/// Maximum bytes of a scheduled task's display name.
///
/// Mirrors the column `CHECK (octet_length(name) BETWEEN 1 AND 200)`
/// in migration 11. Sized for short, agent-authored labels like
/// "morning email check" or "Friday retro prep" — long enough to be
/// descriptive, short enough not to bloat the listing tool result.
pub const SCHEDULED_TASK_NAME_MAX_LEN: usize = 200;

/// Hard cap on simultaneously-active scheduled tasks per agent.
///
/// Bounds the working set the scheduler scans on every poll and stops
/// a runaway agent from filling the table. Sized so a single user's
/// reasonable assistant patterns (~daily/weekly + a few one-shots)
/// fit comfortably.
pub const MAX_SCHEDULED_TASKS_PER_AGENT: usize = 50;

/// Maximum future horizon for a `Once` schedule, measured from now at
/// schedule-creation time.
///
/// Keeps the table from accumulating tasks that will never realistically
/// fire (typo'd dates years in the future) and gives ops a bounded
/// upper limit on how far back a single audit must scan.
pub const MAX_ONESHOT_HORIZON_DAYS: i64 = 365;

/// Polling cadence for the [`super::scheduler::ScheduledTaskScheduler`].
///
/// Trades scheduler precision for DB load — at 30s the worst-case fire
/// latency is one poll interval, well within the granularity humans
/// expect from "every morning at 5am". Tighter cadences would burn DB
/// reads without improving any human-perceptible UX.
pub const SCHEDULED_TASK_POLL_SECS: u64 = 30;

/// Per-tick batch limit on the scheduler's claim query.
///
/// Caps the work a single tick can issue to the prompt queue so a
/// surge of due tasks (e.g. process restarted after long downtime)
/// drains gradually across multiple ticks rather than overloading
/// the queue in one shot.
pub const SCHEDULED_TASK_BATCH_LIMIT: usize = 20;

/// Process-wide fallback timezone applied to `Recurring` schedules.
///
/// Used when the tool call omits `tz`. Today this is just UTC; the
/// `Settings.default_timezone` field overrides it. Surfaced as a
/// `chrono_tz::Tz` so the rest of the subsystem never sees a string
/// here.
pub const DEFAULT_RECURRING_TIMEZONE: Tz = Tz::UTC;

/// Convenience used by the scheduler when converting the poll cadence
/// from a const `u64` into the [`Duration`] the [`super::ScheduledTask`]
/// scaffold accepts. Lives next to the cadence so a future bump touches
/// one site.
#[must_use]
pub(super) const fn scheduled_task_poll_interval() -> Duration {
    Duration::from_secs(SCHEDULED_TASK_POLL_SECS)
}
