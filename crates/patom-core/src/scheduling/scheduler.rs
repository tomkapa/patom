//! Background scheduler for the scheduling subsystem (the third trigger
//! source, doc/thread-chat-refactor.md §2/§8).
//!
//! Polls the `scheduled_tasks` table on a fixed cadence; for each row with
//! `state='active'` and `next_run_at <= now()` the scheduler *initiates a
//! thread* in the task's target channel and wakes the owning agent in it:
//!
//! 1. create a thread in `channel_id` (or a DM when `None`), owned by the
//!    task's human;
//! 2. resolve the agent's `(thread, agent)` participation;
//! 3. seed the task prompt as an owner-private `system_note` the agent reads
//!    read-at-run;
//! 4. enqueue a `Normal` chat trigger (`root_request_id = None` ⇒ fresh DAG
//!    budget), addressed agent-side, sender = the task's human;
//! 5. advance the task's cursor via [`ScheduledTaskStore::record_fired`].
//!
//! The trigger looks like a normal human-initiated chat turn to the worker
//! pool — no new queue kind, no special worker dispatch. Idempotency stays
//! `sched-{task_id}-{fire_ts}`; the agent's eventual `send_message` to the
//! task's human is gated by channel membership like any other delivery.

use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::auth::Caller;
use crate::clock::SharedClock;
use crate::outbound::SharedOutboundRouter;
use crate::provider::{ChatMessage, UserContent};
use crate::runtime::{IdempotencyKey, NewTrigger, RequestKindPayload, SharedPromptQueue};
use crate::threads::{MessageKind, NewMessage, SharedThreadStore};

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
        threads: SharedThreadStore,
        colleagues: crate::colleagues::SharedColleagueStore,
        outbound: SharedOutboundRouter,
        clock: SharedClock,
        parent: CancellationToken,
    ) -> Self {
        Self::spawn_with_cadence(
            store,
            queue,
            threads,
            colleagues,
            outbound,
            clock,
            scheduled_task_poll_interval(),
            Some(parent),
        )
    }

    /// Spawn with an explicit poll cadence — tests use this to avoid
    /// waiting the production 30s; production callers use [`Self::spawn`].
    #[must_use]
    #[allow(clippy::too_many_arguments)]
    pub fn spawn_with_cadence(
        store: SharedScheduledTaskStore,
        queue: SharedPromptQueue,
        threads: SharedThreadStore,
        colleagues: crate::colleagues::SharedColleagueStore,
        outbound: SharedOutboundRouter,
        clock: SharedClock,
        poll_interval: Duration,
        parent: Option<CancellationToken>,
    ) -> Self {
        let inner = Arc::new(SchedulerInner {
            store,
            queue,
            threads,
            colleagues,
            outbound,
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
    threads: SharedThreadStore,
    colleagues: crate::colleagues::SharedColleagueStore,
    /// Attaches the outbound surface pump for a fired thread (#178). Without
    /// this the agent's reply reaches only the web feed — the original
    /// "scheduled task ran, nothing on Lark/Discord" bug.
    outbound: SharedOutboundRouter,
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
        // §5 — bound the directory read so a stuck lookup can't wedge the
        // firing loop. Timeout and inner error both surface as Backend; the
        // row keeps its cursor and the next poll retries. Tenancy flows off the
        // row (`org_id`/`created_by_user_id`); the human owns the new thread.
        let human = tokio::time::timeout(
            COLLEAGUE_RESOLVE_TIMEOUT,
            self.colleagues
                .resolve_user(task.org_id, task.created_by_user_id),
        )
        .await
        .map_err(|_| ScheduledTaskError::Backend("resolve human colleague: timeout".to_string()))?
        .map_err(|e| ScheduledTaskError::Backend(format!("resolve human colleague: {e}")))?;

        // Initiate the thread + agent participation + seed instruction, then
        // wake the agent with a fresh-DAG `Normal` chat trigger.
        let caller = Caller::new(task.created_by_user_id, task.org_id);
        let (thread, state, seed) = self.initiate_thread(task, &caller, human).await?;
        let key = IdempotencyKey::try_from(format!("sched-{}-{}", task.id, fire_at.timestamp()))?;
        let request_id = self
            .queue
            .enqueue_trigger(NewTrigger {
                org_id: task.org_id,
                acting_user_id: task.created_by_user_id,
                thread_id: Some(thread),
                state_id: Some(state),
                background_turn_id: None,
                sender_colleague_id: human,
                receiver_agent_id: task.owner_agent_id,
                root_request_id: None,
                trigger_message_id: Some(seed),
                idempotency_key: key,
                kind_payload: RequestKindPayload::Normal {},
            })
            .await?;

        // Attach outbound delivery for the freshly-created thread so the
        // agent's reply reaches the task's external surface (Lark/Discord),
        // not just the web feed. Best-effort — the composite logs per-surface
        // failures and a web-only thread is a no-op. This is the fix for
        // "scheduled task ran, nothing on Lark/Discord" (#178).
        if let Err(e) = self.outbound.ensure_delivery(task.org_id, thread).await {
            warn!(
                error = ?e,
                patom.thread.id = %thread,
                "scheduling.ensure_delivery.error",
            );
        }

        // Advance the cursor from the materialised schedule against `now`, not
        // the stored cursor — keeps cadence anchored to wall time rather than
        // amplifying scheduler skew across firings.
        let next = task.schedule.next_after(now);
        self.store
            .record_fired(task.id, request_id, fire_at, next)
            .await?;

        let next_str = next.map_or_else(|| "none".to_string(), |t| t.to_rfc3339());
        info!(
            patom.scheduled_task.id = %task.id,
            patom.agent.id = %task.owner_agent_id,
            patom.thread.id = %thread,
            patom.scheduled_task.fire_at = %fire_at,
            patom.request.id = %request_id,
            patom.scheduled_task.next_run_at = %next_str,
            "scheduling.fired",
        );
        Ok(())
    }

    /// Create the thread in the task's channel (DM when `None`), resolve the
    /// agent's participation, and seed the task prompt as an owner-private
    /// `system_note`. Returns `(thread, state, seed_message)`.
    async fn initiate_thread(
        &self,
        task: &ScheduledTaskRecord,
        caller: &Caller,
        human: crate::colleagues::ColleagueId,
    ) -> Result<
        (
            crate::threads::ThreadId,
            crate::threads::AgentThreadId,
            crate::threads::ThreadMessageId,
        ),
        ScheduledTaskError,
    > {
        // A channel task posts into its channel; a channel-less task is a DM
        // between the owner human and the task's agent (the DM counterpart).
        let counterpart = match task.channel_id {
            Some(_) => None,
            None => Some(
                self.colleagues
                    .resolve_agent(task.org_id, task.owner_agent_id)
                    .await
                    .map_err(|e| {
                        ScheduledTaskError::Backend(format!("resolve agent colleague: {e}"))
                    })?,
            ),
        };
        let thread = self
            .threads
            .create_thread(caller, task.channel_id, None, human, counterpart)
            .await?;
        // Once the thread exists, the participation row (`agent_thread_state`)
        // and the seed message (`thread_messages`) are independent — disjoint
        // tables, no ordering dep — so overlap them to save a round-trip.
        let (state, seed) = tokio::try_join!(
            self.threads
                .resolve_participation(caller, thread, task.owner_agent_id),
            self.threads.append(
                caller,
                thread,
                NewMessage {
                    kind: MessageKind::SystemNote,
                    sender: None,
                    owner_agent_id: Some(task.owner_agent_id),
                    receiver: None,
                    body: ChatMessage::User(vec![UserContent::Text(seed_prompt_text(task))]),
                    request_id: None,
                    idempotency_key: None,
                },
            )
        )?;
        Ok((thread, state, seed))
    }
}

/// Build the seeded instruction text for a fired task.
///
/// A channel task (the digest shape, #199) gets a deterministic digest-window
/// footer carrying the `since` cursor: the previous fire (`last_fired_at`), or
/// the task's creation when it has never fired. The agent passes this to
/// `read_channel` so a digest summarises only what is new since the last run.
/// The schedule cursor advances *after* this seed (`record_fired`), so
/// `last_fired_at` here is the prior fire, not this one; a retried fire reuses
/// the same value (the `sched-{id}-{ts}` idempotency key dedups the trigger). A
/// DM task carries no channel to read, so its prompt is seeded unchanged.
fn seed_prompt_text(task: &ScheduledTaskRecord) -> String {
    let base = task.prompt.as_str();
    if task.channel_id.is_none() {
        return base.to_string();
    }
    let since = task.last_fired_at.unwrap_or(task.created_at);
    format!(
        "{base}\n\n<digest-window since=\"{}\"/>\n\
         Messages since the last run are those at or after this timestamp; \
         pass it as `since` when you read a channel.",
        since.to_rfc3339()
    )
}

#[cfg(test)]
mod tests {
    use chrono::{TimeZone, Utc};

    use super::seed_prompt_text;
    use crate::agents::AgentId;
    use crate::auth::{OrgId, UserId};
    use crate::channels::ChannelId;
    use crate::scheduling::{
        ScheduleSpec, ScheduledPrompt, ScheduledTaskId, ScheduledTaskName, ScheduledTaskRecord,
        ScheduledTaskState,
    };

    fn task(channel: Option<ChannelId>) -> ScheduledTaskRecord {
        let created = Utc
            .with_ymd_and_hms(2026, 6, 1, 8, 0, 0)
            .single()
            .expect("ts");
        ScheduledTaskRecord {
            id: ScheduledTaskId::new(),
            owner_agent_id: AgentId::new(),
            org_id: OrgId::new(),
            created_by_user_id: UserId::new(),
            channel_id: channel,
            name: ScheduledTaskName::try_from("digest").expect("name"),
            prompt: ScheduledPrompt::try_from("Summarise the channel.").expect("prompt"),
            schedule: ScheduleSpec::Once { run_at: created },
            next_run_at: None,
            last_fired_at: None,
            last_request_id: None,
            state: ScheduledTaskState::Active,
            created_at: created,
            updated_at: created,
        }
    }

    #[test]
    fn dm_task_seeds_prompt_unchanged() {
        let t = task(None);
        assert_eq!(seed_prompt_text(&t), "Summarise the channel.");
        assert!(!seed_prompt_text(&t).contains("digest-window"));
    }

    #[test]
    fn first_fire_anchors_window_on_creation() {
        let t = task(Some(ChannelId::new()));
        let out = seed_prompt_text(&t);
        assert!(
            out.contains("Summarise the channel."),
            "keeps the base prompt"
        );
        assert!(
            out.contains("since=\"2026-06-01T08:00:00+00:00\""),
            "an unfired task anchors the window on its creation, got: {out}"
        );
    }

    #[test]
    fn later_fire_anchors_window_on_previous_fire() {
        let mut t = task(Some(ChannelId::new()));
        let prev = Utc
            .with_ymd_and_hms(2026, 6, 16, 9, 0, 0)
            .single()
            .expect("ts");
        t.last_fired_at = Some(prev);
        let out = seed_prompt_text(&t);
        assert!(
            out.contains("since=\"2026-06-16T09:00:00+00:00\""),
            "a fired task anchors the window on the previous fire, got: {out}"
        );
        assert!(
            !out.contains("2026-06-01"),
            "the creation time is not used once the task has fired"
        );
    }
}
