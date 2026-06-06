//! Storage trait + cheap-clone handle for the scheduling subsystem.
//!
//! Tools and the scheduler depend on this trait — never on the concrete
//! Postgres impl in [`super::pg_store`]. The async-trait surface stays
//! object-safe so future backends (in-memory test fakes, alternative
//! databases) drop in.

use std::fmt;
use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};

use crate::agents::AgentId;
use crate::auth::{OrgId, UserId};
use crate::runtime::PromptRequestId;

use super::error::ScheduledTaskError;
use super::types::{
    ScheduleSpec, ScheduledPrompt, ScheduledTaskId, ScheduledTaskName, ScheduledTaskRecord,
};

/// Input to [`ScheduledTaskStore::create`].
///
/// Server-side fields (`id`, `created_at`, `updated_at`, `state`) are
/// minted by the store; `next_run_at` is computed by the tool boundary
/// so the inserted row is immediately due-pollable.
///
/// `org_id` is the owning organisation (sourced from the calling agent's
/// `agents.org_id`); `created_by_user_id` is the human at the DAG root
/// of the session that produced the `schedule_task` tool call. Both are
/// load-bearing — the scheduler reads them off the row at fire-time and
/// pins the enqueued `prompt_requests` row to the same tenant, and the
/// parity trigger on `scheduled_tasks` (migration 19) rejects an INSERT
/// whose `org_id` ≠ the owning agent's org.
#[derive(Debug, Clone)]
pub struct NewScheduledTask {
    pub owner_agent_id: AgentId,
    pub org_id: OrgId,
    pub created_by_user_id: UserId,
    pub name: ScheduledTaskName,
    pub prompt: ScheduledPrompt,
    pub schedule: ScheduleSpec,
    pub next_run_at: Option<DateTime<Utc>>,
}

/// HTTP-PATCH-style update payload. Today only the scheduler uses this
/// (to advance `next_run_at` after a fire); kept generic so a future
/// "edit task" tool drops in without a new method.
#[derive(Debug, Clone, Default)]
pub struct ScheduledTaskUpdate {
    pub next_run_at: Option<Option<DateTime<Utc>>>,
    pub last_fired_at: Option<DateTime<Utc>>,
    pub last_request_id: Option<PromptRequestId>,
    /// `Some(state)` to transition to the wrapped state (`Done`,
    /// `Cancelled`); `None` keeps the current state.
    pub state: Option<super::types::ScheduledTaskState>,
}

#[async_trait]
pub trait ScheduledTaskStore: fmt::Debug + Send + Sync {
    /// Mint a new task row.
    async fn create(
        &self,
        payload: NewScheduledTask,
    ) -> Result<ScheduledTaskRecord, ScheduledTaskError>;

    /// Tenant-scoped variant of [`Self::create`]. Opens
    /// `begin_as_user(acting_user_id)` so the `scheduled_tasks`
    /// INSERT runs RLS-checked. The `schedule_task` tool sources
    /// `acting_user_id` from the session's
    /// `created_by_user_id`; the scheduler / HTTP paths stay on
    /// the privileged entry point.
    async fn create_for_user(
        &self,
        acting_user_id: UserId,
        payload: NewScheduledTask,
    ) -> Result<ScheduledTaskRecord, ScheduledTaskError>;

    /// Snapshot of every active task for `owner`, ordered by
    /// `created_at` ascending. Cancelled / done rows are excluded —
    /// the agent only manages its live tasks.
    async fn list_for_agent(
        &self,
        owner: AgentId,
    ) -> Result<Vec<ScheduledTaskRecord>, ScheduledTaskError>;

    /// Cancel a task. Returns [`ScheduledTaskError::NotFound`] when the
    /// row is absent or owned by a different agent — the two cases are
    /// folded together so the tool seam can't be used to probe for the
    /// existence of other agents' tasks. Idempotent on already-
    /// cancelled / done rows.
    async fn cancel(&self, task: ScheduledTaskId, owner: AgentId)
    -> Result<(), ScheduledTaskError>;

    /// Tenant-scoped variant of [`Self::cancel`].
    async fn cancel_for_user(
        &self,
        acting_user_id: UserId,
        task: ScheduledTaskId,
        owner: AgentId,
    ) -> Result<(), ScheduledTaskError>;

    /// Read due rows for the scheduler. Plain SELECT (no row-level
    /// locking); concurrent scheduler nodes dedupe at the queue layer
    /// via the `sched-{task_id}-{fire_ts}` idempotency key.
    async fn claim_due(
        &self,
        now: DateTime<Utc>,
        limit: usize,
    ) -> Result<Vec<ScheduledTaskRecord>, ScheduledTaskError>;

    /// Record the outcome of a fire: bump `last_fired_at` /
    /// `last_request_id`, advance `next_run_at`, and flip `state` to
    /// `Done` when the schedule is exhausted.
    async fn record_fired(
        &self,
        task: ScheduledTaskId,
        request_id: PromptRequestId,
        fired_at: DateTime<Utc>,
        next_run_at: Option<DateTime<Utc>>,
    ) -> Result<(), ScheduledTaskError>;
}

/// Cheap-clone handle so collaborators hold the store without a generic
/// parameter.
pub type SharedScheduledTaskStore = Arc<dyn ScheduledTaskStore>;
