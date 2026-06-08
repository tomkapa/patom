//! Background scheduler for the scheduling subsystem.
//!
//! Polls the `scheduled_tasks` table on a fixed cadence; for each
//! row with `state='active'` and `next_run_at <= now()` it enqueues a
//! `prompt_requests` row addressed to the owning agent and advances
//! the task's cursor via [`ScheduledTaskStore::record_fired`].
//!
//! The fired prompt looks like a normal human-initiated turn from the
//! worker pool's point of view — `sender = Participant::Human`,
//! `parent_session: None`, `kind_payload: Normal`. No new queue kind,
//! no special worker dispatch.

use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::clock::SharedClock;
use crate::runtime::{IdempotencyKey, NewPromptRequest, SharedPromptQueue};
use crate::types::{Participant, Prompt};

use super::error::ScheduledTaskError;
use super::limits::{
    COLLEAGUE_RESOLVE_TIMEOUT, SCHEDULED_TASK_BATCH_LIMIT, scheduled_task_poll_interval,
};
use super::scheduled_task::ScheduledTask;
use super::store::SharedScheduledTaskStore;
use super::types::ScheduledTaskRecord;

/// Polls due rows out of the scheduled_tasks table and onto the prompt
/// queue. Owned by [`Server`](crate::app::Server); shutdown winds the
/// task down on Ctrl+C via the parent token.
#[derive(Debug)]
pub struct ScheduledTaskScheduler {
    task: ScheduledTask,
}

impl ScheduledTaskScheduler {
    /// Spawn with the production poll cadence
    /// ([`scheduled_task_poll_interval`]). The supplied parent token
    /// wires shutdown into the main runtime Ctrl+C signal.
    #[must_use]
    pub fn spawn(
        store: SharedScheduledTaskStore,
        queue: SharedPromptQueue,
        colleagues: crate::colleagues::SharedColleagueStore,
        clock: SharedClock,
        parent: CancellationToken,
    ) -> Self {
        Self::spawn_with_cadence(
            store,
            queue,
            colleagues,
            clock,
            scheduled_task_poll_interval(),
            Some(parent),
        )
    }

    /// Spawn with an explicit poll cadence — tests use this to avoid
    /// waiting the production 30s; production callers use [`Self::spawn`].
    #[must_use]
    pub fn spawn_with_cadence(
        store: SharedScheduledTaskStore,
        queue: SharedPromptQueue,
        colleagues: crate::colleagues::SharedColleagueStore,
        clock: SharedClock,
        poll_interval: Duration,
        parent: Option<CancellationToken>,
    ) -> Self {
        let inner = Arc::new(SchedulerInner {
            store,
            queue,
            colleagues,
            clock,
            batch_limit: SCHEDULED_TASK_BATCH_LIMIT,
        });
        let task = ScheduledTask::spawn("scheduling.scheduler", poll_interval, parent, move || {
            let inner = inner.clone();
            async move { inner.tick().await }
        });
        Self { task }
    }

    pub async fn shutdown(self) {
        self.task.shutdown().await;
    }
}

#[derive(Debug)]
struct SchedulerInner {
    store: SharedScheduledTaskStore,
    queue: SharedPromptQueue,
    colleagues: crate::colleagues::SharedColleagueStore,
    clock: SharedClock,
    batch_limit: usize,
}

impl SchedulerInner {
    #[tracing::instrument(
        skip_all,
        name = "scheduling.tick",
        fields(
            patom.scheduled_task.due_count = tracing::field::Empty,
            patom.scheduled_task.fired_count = tracing::field::Empty,
        ),
    )]
    async fn tick(&self) -> Result<(), ScheduledTaskError> {
        let now: DateTime<Utc> = self.clock.now_wall().into();
        let due = self.store.claim_due(now, self.batch_limit).await?;
        tracing::Span::current().record("patom.scheduled_task.due_count", due.len());

        let mut fired = 0usize;
        for task in due {
            match self.fire(&task, now).await {
                Ok(()) => fired += 1,
                Err(e) => warn!(
                    error = %e,
                    patom.scheduled_task.id = %task.id,
                    patom.agent.id = %task.owner_agent_id,
                    "scheduling.fire.error",
                ),
            }
        }
        tracing::Span::current().record("patom.scheduled_task.fired_count", fired);
        Ok(())
    }

    async fn fire(
        &self,
        task: &ScheduledTaskRecord,
        now: DateTime<Utc>,
    ) -> Result<(), ScheduledTaskError> {
        let fire_at = task.next_run_at.unwrap_or(now);
        let prompt = Prompt::try_from(task.prompt.as_str().to_string())?;
        let key = IdempotencyKey::try_from(format!("sched-{}-{}", task.id, fire_at.timestamp()))?;
        // Tenancy flows directly off the row — migration 19 added
        // `org_id` + `created_by_user_id` to `scheduled_tasks`, so the
        // scheduler no longer needs to JOIN through `agents` /
        // `org_members` at fire-time.
        // §5 — bound the directory read so a stuck lookup can't wedge the
        // firing loop. Timeout and inner error both surface as Backend; the
        // row keeps its cursor and the next poll retries.
        let human_colleague = tokio::time::timeout(
            COLLEAGUE_RESOLVE_TIMEOUT,
            self.colleagues
                .resolve_user(task.org_id, task.created_by_user_id),
        )
        .await
        .map_err(|_| ScheduledTaskError::Backend("resolve human colleague: timeout".to_string()))?
        .map_err(|e| ScheduledTaskError::Backend(format!("resolve human colleague: {e}")))?;
        let req = NewPromptRequest::normal(
            None,
            Participant::human(human_colleague, task.created_by_user_id),
            task.owner_agent_id,
            None,
            prompt,
            key,
            task.org_id,
            task.created_by_user_id,
        );
        let outcome = self.queue.enqueue(req).await?;
        let request_id = outcome.request_id();

        // Compute next fire from the materialised schedule against `now`,
        // not the stored cursor — keeps the cadence anchored to wall time
        // rather than amplifying scheduler skew across firings.
        let next = task.schedule.next_after(now);
        self.store
            .record_fired(task.id, request_id, fire_at, next)
            .await?;

        let next_str = next.map_or_else(|| "none".to_string(), |t| t.to_rfc3339());
        info!(
            patom.scheduled_task.id = %task.id,
            patom.agent.id = %task.owner_agent_id,
            patom.scheduled_task.fire_at = %fire_at,
            patom.request.id = %request_id,
            patom.scheduled_task.next_run_at = %next_str,
            "scheduling.fired",
        );
        Ok(())
    }
}
