//! Worker pool. A bounded `JoinSet` of N tasks, each running a claim-and-run loop
//! over the thread-feed trigger queue:
//!
//! ```text
//! loop {
//!   match queue.claim_next_turn(worker_id) {
//!     Some(claim) => {
//!       spawn heartbeat on the claim_key lease (renew every TTL/3, dies on drop)
//!       result = timeout(MAX_TURN, agent.reply_in_thread(state, thread, ...))  // read-at-run
//!       ping-pong guard: a turn that posts no message is nudged + retried
//!       mark_turn_done | mark_turn_failed ; release_turn
//!     }
//!     None => sleep(idle_poll)
//!   }
//! }
//! ```

use std::sync::Arc;
use std::time::Duration;

use tokio::task::{JoinHandle, JoinSet};
use tokio::time::timeout;
use tokio_util::sync::{CancellationToken, DropGuard};
use tracing::{Instrument, debug, info, warn};

use async_trait::async_trait;

use crate::agent_core::{Agent, AgentError, SharedTurnObserver, TurnObserver};
use crate::agents::SharedAgents;
use crate::auth::{Caller, UserId};
use crate::background::BackgroundTurnId;
use crate::observability::log::preview;
use crate::provider::{AssistantContent, ChatMessage, ToolResult, UserContent};
use crate::threads::{AgentThreadId, MessageKind, NewMessage, SharedThreadStore, ThreadId};
use crate::types::{AgentReply, Participant};

use super::dag::SharedDagBudget;
use super::limits::{
    CANCEL_POLL_INTERVAL, MAX_PINGPONG_RETRIES, MAX_TURN_DURATION, MAX_WORKERS, WORKER_IDLE_POLL,
};
use super::queue::{ClaimedTurn, LeaseTiming, SharedPromptQueue, TurnReceipt};
use super::response::{ResponseChunk, SharedResponseSink};
use super::types::{FailureReason, PromptRequestId, RequestKind, RequestKindPayload, WorkerId};

/// System nudge appended (owner-private) to the agent's feed view when it
/// emitted a turn without posting a message via `send_message`.
const PINGPONG_NUDGE: &str = "you produced text without calling send_message; \
    the message was not delivered. Call send_message to communicate.";

/// Construction-time configuration for the pool.
///
/// `lease_timing` is shared with
/// [`PgPromptQueue::with_caps`](super::pg_queue::PgPromptQueue::with_caps) so
/// the worker's heartbeat cadence stays co-validated with the queue's TTL.
#[derive(Debug, Clone)]
pub struct WorkerConfig {
    pub workers: usize,
    pub lease_timing: LeaseTiming,
    pub max_turn_duration: Duration,
    pub idle_poll: Duration,
    pub cancel_poll: Duration,
}

impl Default for WorkerConfig {
    fn default() -> Self {
        Self {
            workers: MAX_WORKERS,
            lease_timing: LeaseTiming::default_const(),
            max_turn_duration: MAX_TURN_DURATION,
            idle_poll: WORKER_IDLE_POLL,
            cancel_poll: CANCEL_POLL_INTERVAL,
        }
    }
}

/// Handle returned by [`WorkerPool::spawn`]. Drop or call `shutdown().await`
/// to wind down — CLAUDE.md §7 forbids floating tasks.
#[derive(Debug)]
pub struct WorkerPoolHandle {
    shutdown: DropGuard,
    workers: JoinSet<()>,
}

impl WorkerPoolHandle {
    /// Signal every worker to stop and await all of them. Idempotent.
    pub async fn shutdown(mut self) {
        drop(self.shutdown);
        while let Some(joined) = self.workers.join_next().await {
            if let Err(e) = joined {
                warn!(error = %e, "worker.join.error");
            }
        }
    }
}

#[derive(Debug)]
pub struct WorkerPool {
    queue: SharedPromptQueue,
    sink: SharedResponseSink,
    agents: SharedAgents,
    threads: SharedThreadStore,
    dag: SharedDagBudget,
    cfg: WorkerConfig,
}

impl WorkerPool {
    #[must_use]
    pub fn new(
        queue: SharedPromptQueue,
        sink: SharedResponseSink,
        agents: SharedAgents,
        threads: SharedThreadStore,
        dag: SharedDagBudget,
        cfg: WorkerConfig,
    ) -> Self {
        Self {
            queue,
            sink,
            agents,
            threads,
            dag,
            cfg,
        }
    }

    /// Spawn `cfg.workers` tasks into a bounded `JoinSet` and return a handle whose
    /// drop / shutdown cleanly winds them down.
    #[must_use]
    pub fn spawn(self) -> WorkerPoolHandle {
        let mut set = JoinSet::new();
        let cfg = self.cfg.clone();
        let workers = cfg.workers.max(1);
        let shutdown = CancellationToken::new();

        for _ in 0..workers {
            let worker = Worker {
                id: WorkerId::new(),
                queue: self.queue.clone(),
                sink: self.sink.clone(),
                agents: self.agents.clone(),
                threads: self.threads.clone(),
                dag: self.dag.clone(),
                cfg: cfg.clone(),
                shutdown: shutdown.clone(),
            };
            set.spawn(async move { worker.run().await });
        }

        WorkerPoolHandle {
            shutdown: shutdown.drop_guard(),
            workers: set,
        }
    }
}

#[derive(Debug, Clone)]
struct Worker {
    id: WorkerId,
    queue: SharedPromptQueue,
    sink: SharedResponseSink,
    agents: SharedAgents,
    threads: SharedThreadStore,
    dag: SharedDagBudget,
    cfg: WorkerConfig,
    shutdown: CancellationToken,
}

impl Worker {
    /// Worker's main loop. Not wrapped in `#[instrument]` — the span would
    /// outlive the worker and orphan every per-claim child span. Letting
    /// `handle_turn` be the trace root keeps each turn on one trace.
    async fn run(self) {
        loop {
            if self.shutdown.is_cancelled() {
                debug!(patom.worker.id = %self.id, "worker.shutdown");
                return;
            }
            match self.queue.claim_next_turn(self.id).await {
                Ok(Some(claim)) => self.handle_turn(claim).await,
                Ok(None) => self.idle().await,
                Err(e) => {
                    warn!(patom.worker.id = %self.id, error = %e, "worker.claim.error");
                    self.idle().await;
                }
            }
        }
    }

    async fn idle(&self) {
        tokio::select! {
            biased;
            () = self.shutdown.cancelled() => {},
            () = tokio::time::sleep(self.cfg.idle_poll) => {},
        }
    }

    async fn handle_turn(&self, claim: ClaimedTurn) {
        let span = tracing::info_span!(
            "worker.handle_turn",
            patom.worker.id = %self.id,
            patom.thread.id = ?claim.thread_id,
            patom.state.id = %claim.claim_key,
            patom.agent.id = %claim.receiver_agent_id,
            patom.batch_size = claim.trigger_ids.len(),
            patom.lease_seq = claim.lease_seq.get(),
        );
        self.handle_turn_inner(claim).instrument(span).await;
    }

    async fn handle_turn_inner(&self, claim: ClaimedTurn) {
        let receipt = Arc::new(claim.receipt());
        let cancel = CancellationToken::new();

        if self.any_cancelled(receipt.trigger_ids()).await {
            self.publish_failure(&receipt, &FailureReason::Cancelled)
                .await;
            self.finalise(&receipt, FailureReason::Cancelled).await;
            self.release(&receipt).await;
            return;
        }

        // Resolve the agent before spawning watchers so an unknown id fails
        // fast without holding a lease.
        let agent = match self.agents.get(claim.receiver_agent_id).await {
            Ok(a) => a,
            Err(e) => {
                warn!(error = %e, patom.agent.id = %claim.receiver_agent_id, "worker.agent.resolve.error");
                let reason = FailureReason::Unrecoverable(format!("agent resolve: {e}"));
                self.publish_failure(&receipt, &reason).await;
                self.finalise(&receipt, reason).await;
                self.release(&receipt).await;
                return;
            }
        };

        let heartbeat = self.spawn_heartbeat(receipt.clone());
        let cancel_watcher = self.spawn_cancel_watcher(receipt.clone(), cancel.clone());

        match claim.kind {
            RequestKind::Normal => {
                let observer: SharedTurnObserver = Arc::new(FanOutObserver {
                    sink: self.sink.clone(),
                    user_id: receipt.acting_user_id(),
                    ids: receipt.trigger_ids().to_vec(),
                });
                self.run_with_pingpong_guard(&agent, &claim, &receipt, cancel.clone(), observer)
                    .await;
            }
            RequestKind::Reflection | RequestKind::Resolution => {
                self.run_background(&agent, &claim, &receipt, cancel.clone())
                    .await;
            }
        }

        cancel_watcher.abort();
        let _ = cancel_watcher.await;
        heartbeat.abort();
        let _ = heartbeat.await;
        self.release(&receipt).await;
    }

    /// Drive the chat turn, retrying with a nudge when the agent produces text
    /// without posting a message (the force-exit guard). The egress signal is a
    /// `send_message` call landing a posted feed row — `AgentReply::delivered`.
    async fn run_with_pingpong_guard(
        &self,
        agent: &Agent,
        claim: &ClaimedTurn,
        receipt: &Arc<TurnReceipt>,
        cancel: CancellationToken,
        observer: SharedTurnObserver,
    ) {
        let mut retries: u8 = 0;
        loop {
            let outcome = self
                .run_one_attempt(agent, claim, cancel.clone(), observer.clone())
                .await;
            match outcome {
                Ok(Ok(reply)) if reply.send_message_calls() == 0 => {
                    if retries >= MAX_PINGPONG_RETRIES {
                        warn!(
                            patom.state.id = %claim.claim_key,
                            patom.pingpong.retries = retries,
                            text.preview = %preview(reply.final_text()),
                            "worker.turn.no_egress.exceeded",
                        );
                        self.publish_failure(receipt, &FailureReason::NoEgress)
                            .await;
                        self.finalise(receipt, FailureReason::NoEgress).await;
                        return;
                    }
                    retries += 1;
                    info!(
                        patom.state.id = %claim.claim_key,
                        patom.pingpong.retries = retries,
                        "worker.turn.no_egress.retried",
                    );
                    if let Err(e) = self.inject_pingpong_nudge(claim, receipt).await {
                        warn!(error = %e, "worker.pingpong.nudge.error");
                        let reason = FailureReason::Unrecoverable(format!("nudge append: {e}"));
                        self.publish_failure(receipt, &reason).await;
                        self.finalise(receipt, reason).await;
                        return;
                    }
                }
                Ok(Ok(reply)) => {
                    self.handle_success(receipt, reply).await;
                    return;
                }
                Ok(Err(e)) => {
                    self.handle_agent_error(receipt, e).await;
                    return;
                }
                Err(_elapsed) => {
                    warn!(patom.state.id = %claim.claim_key, "worker.turn.timeout");
                    self.publish_failure(receipt, &FailureReason::Timeout).await;
                    self.finalise(receipt, FailureReason::Timeout).await;
                    return;
                }
            }
        }
    }

    /// Run a background-cognition turn (reflection / resolution). No ping-pong
    /// guard — the turn may legitimately end without `send_message` — and the
    /// exchange lands in the background store, never the chat feed. On success
    /// the trigger is marked done and, for a reflection, the checkpoint is
    /// advanced so the scheduler does not re-enqueue the same idle window.
    async fn run_background(
        &self,
        agent: &Agent,
        claim: &ClaimedTurn,
        receipt: &TurnReceipt,
        cancel: CancellationToken,
    ) {
        let viewer = Participant::agent(claim.receiver_colleague_id, claim.receiver_agent_id);
        let outcome = timeout(
            self.cfg.max_turn_duration,
            agent.reply_background(
                BackgroundTurnId::from(claim.claim_key),
                viewer,
                lead_request_id(claim),
                Caller::new(claim.acting_user_id, claim.org_id),
                claim.kind_payload.clone(),
                cancel,
                None,
            ),
        )
        .await;
        match outcome {
            // The reply text is unused — a cognition turn has no SSE consumer;
            // the artifacts already landed in the background store.
            Ok(Ok(_)) => {
                info!(
                    patom.state.id = %claim.claim_key,
                    patom.request.kind = claim.kind.as_str(),
                    "worker.background.ok",
                );
                self.advance_reflection_checkpoint(claim).await;
                if let Err(e) = self.queue.mark_turn_done(receipt).await {
                    warn!(error = %e, "worker.background.mark_turn_done.error");
                }
                self.maybe_emit_quiescence(receipt).await;
            }
            Ok(Err(e)) => self.handle_agent_error(receipt, e).await,
            Err(_elapsed) => {
                warn!(patom.state.id = %claim.claim_key, "worker.background.timeout");
                self.finalise(receipt, FailureReason::Timeout).await;
            }
        }
    }

    /// On a successful reflection, advance `reflection_checkpoints (agent,
    /// thread)` to the frozen slice's `up_to_message_id` so the scheduler picks
    /// up strictly after it. No-op for resolution (no checkpoint). Best-effort:
    /// a checkpoint-write failure only means a duplicate reflection next tick,
    /// never a lost turn — log and move on.
    async fn advance_reflection_checkpoint(&self, claim: &ClaimedTurn) {
        let RequestKindPayload::Reflection {
            thread_id,
            up_to_message_id,
        } = &claim.kind_payload
        else {
            return;
        };
        if let Err(e) = self
            .threads
            .advance_reflection_checkpoint(
                claim.org_id,
                claim.receiver_agent_id,
                *thread_id,
                *up_to_message_id,
            )
            .await
        {
            warn!(error = %e, patom.thread.id = %thread_id, "worker.reflection.checkpoint.error");
        }
    }

    /// One attempt at the chat turn. Read-at-run: every attempt re-reads the
    /// feed (so a nudge appended between retries is seen), so there is no
    /// `reply`/`resume` distinction — each attempt is a fresh `reply_in_thread`.
    async fn run_one_attempt(
        &self,
        agent: &Agent,
        claim: &ClaimedTurn,
        cancel: CancellationToken,
        observer: SharedTurnObserver,
    ) -> Result<Result<AgentReply, AgentError>, tokio::time::error::Elapsed> {
        let viewer = Participant::agent(claim.receiver_colleague_id, claim.receiver_agent_id);
        timeout(
            self.cfg.max_turn_duration,
            agent.reply_in_thread(
                AgentThreadId::from(claim.claim_key),
                self.thread_of(claim),
                viewer,
                lead_request_id(claim),
                claim.root_request_id,
                Caller::new(claim.acting_user_id, claim.org_id),
                RequestKindPayload::Normal {},
                cancel,
                Some(observer),
            ),
        )
        .await
    }

    /// Append the ping-pong nudge as an owner-private `system_note` so the agent
    /// re-reads it on its next attempt (peers never see it).
    async fn inject_pingpong_nudge(
        &self,
        claim: &ClaimedTurn,
        receipt: &TurnReceipt,
    ) -> Result<(), super::error::PromptError> {
        let caller = Caller::new(claim.acting_user_id, claim.org_id);
        self.threads
            .append(
                &caller,
                self.thread_of(claim),
                NewMessage {
                    kind: MessageKind::SystemNote,
                    sender: None,
                    owner_agent_id: Some(claim.receiver_agent_id),
                    receiver: None,
                    body: ChatMessage::User(vec![UserContent::Text(PINGPONG_NUDGE.to_string())]),
                    request_id: Some(receipt.root_request_id()),
                    idempotency_key: None,
                },
            )
            .await
            .map(|_| ())
            .map_err(|e| super::error::PromptError::Backend(format!("nudge: {e}")))
    }

    /// Thread a chat turn runs in. A `Normal` claim always carries a
    /// `thread_id`; a missing one is a queue invariant violation (CLAUDE.md §6).
    fn thread_of(&self, claim: &ClaimedTurn) -> ThreadId {
        claim
            .thread_id
            .expect("invariant: a Normal chat trigger always carries a thread_id")
    }

    async fn any_cancelled(&self, ids: &[PromptRequestId]) -> bool {
        match self.queue.statuses(ids).await {
            Ok(views) => views
                .iter()
                .any(|v| v.cancellation_requested || v.status.is_terminal()),
            Err(e) => {
                warn!(error = %e, "worker.status.error");
                false
            }
        }
    }

    async fn handle_success(&self, receipt: &TurnReceipt, reply: AgentReply) {
        info!(
            patom.state.id = %receipt.claim_key(),
            patom.send_message.calls = reply.send_message_calls(),
            text.preview = %preview(reply.final_text()),
            "worker.turn.ok",
        );
        if let Err(e) = self.queue.mark_turn_done(receipt).await {
            warn!(error = %e, "worker.mark_turn_done.error");
        }
        self.maybe_emit_quiescence(receipt).await;
    }

    async fn handle_agent_error(&self, receipt: &TurnReceipt, err: AgentError) {
        // Exhaustive — a new `AgentError` variant must light up here rather
        // than silently falling through to `Provider`.
        let reason = match err {
            AgentError::Cancelled => FailureReason::Cancelled,
            AgentError::ProviderTimeout | AgentError::TodosLoadTimeout => FailureReason::Timeout,
            AgentError::HookDenied(d) => FailureReason::Hook(d.0),
            AgentError::BillingExceeded { .. } => FailureReason::BillingExceeded,
            e @ (AgentError::Provider(_)
            | AgentError::Internal(_)
            | AgentError::Thread(_)
            | AgentError::Background(_)
            | AgentError::Memory(_)
            | AgentError::Todos(_)
            | AgentError::Hook(_)
            | AgentError::ToolTimeout { .. }
            | AgentError::UnknownTool(_)
            | AgentError::TooManyToolCalls { .. }
            | AgentError::MaxTurnsExceeded(_)
            | AgentError::EmptyReply) => FailureReason::Provider(e.to_string()),
        };
        warn!(
            patom.state.id = %receipt.claim_key(),
            reason = reason.label(),
            detail = %reason,
            "worker.turn.error",
        );
        self.publish_failure(receipt, &reason).await;
        self.finalise(receipt, reason).await;
    }

    async fn publish_failure(&self, receipt: &TurnReceipt, reason: &FailureReason) {
        let user_id = receipt.acting_user_id();
        for id in receipt.trigger_ids() {
            if let Err(e) = self
                .sink
                .publish_for_user(user_id, *id, ResponseChunk::from_failure(reason))
                .await
            {
                debug!(error = %e, patom.request.id = %id, "worker.publish.failure.skipped");
            }
            if let Err(e) = self.sink.close_for_user(user_id, *id).await {
                debug!(error = %e, patom.request.id = %id, "worker.sink.close.skipped");
            }
        }
    }

    async fn finalise(&self, receipt: &TurnReceipt, reason: FailureReason) {
        if let Err(e) = self.queue.mark_turn_failed(receipt, reason).await {
            warn!(error = %e, "worker.mark_turn_failed.error");
        }
        self.maybe_emit_quiescence(receipt).await;
    }

    /// Release the `claim_key` lease so the next pending trigger for this
    /// `(thread, agent)` can be claimed without waiting for TTL expiry.
    async fn release(&self, receipt: &TurnReceipt) {
        if let Err(e) = self.queue.release_turn(receipt).await {
            warn!(error = %e, "worker.release_turn.error");
        }
    }

    /// Emit the terminal `Done` chunk on each trigger's sink when no
    /// `pending` / `processing` rows remain in this turn's DAG. Quiescence is
    /// keyed on the claim's DAG root; the chunk is published on the trigger ids
    /// (their sinks are open for the claim's duration). Synthetic ids that never
    /// opened a stream surface as a benign skip.
    async fn maybe_emit_quiescence(&self, receipt: &TurnReceipt) {
        let root = receipt.root_request_id();
        let user_id = receipt.acting_user_id();
        match self.dag.quiescent(root).await {
            Ok(true) => {
                for id in receipt.trigger_ids() {
                    if let Err(e) = self
                        .sink
                        .publish_for_user(
                            user_id,
                            *id,
                            ResponseChunk::Done {
                                final_text: String::new(),
                            },
                        )
                        .await
                    {
                        debug!(error = %e, patom.request.id = %id, "worker.quiescence.publish.skipped");
                        continue;
                    }
                    if let Err(e) = self.sink.close_for_user(user_id, *id).await {
                        debug!(error = %e, patom.request.id = %id, "worker.quiescence.close.skipped");
                    }
                }
                info!(patom.dag.root = %root, "worker.quiescence.done");
            }
            Ok(false) => debug!(patom.dag.root = %root, "worker.quiescence.live"),
            Err(e) => warn!(error = %e, patom.dag.root = %root, "worker.quiescence.query.error"),
        }
    }

    fn spawn_heartbeat(&self, receipt: Arc<TurnReceipt>) -> JoinHandle<()> {
        let queue = self.queue.clone();
        let interval = self.cfg.lease_timing.heartbeat_interval();
        let shutdown = self.shutdown.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    biased;
                    () = shutdown.cancelled() => return,
                    () = tokio::time::sleep(interval) => {},
                }
                if let Err(e) = queue.heartbeat_turn(&receipt).await {
                    debug!(error = %e, "worker.heartbeat.stale");
                    return;
                }
            }
        })
    }

    /// Polls `queue.statuses` for every trigger id; the first observed cancelled
    /// or terminal fires `cancel`. The agent honours it at its next checkpoint.
    fn spawn_cancel_watcher(
        &self,
        receipt: Arc<TurnReceipt>,
        cancel: CancellationToken,
    ) -> JoinHandle<()> {
        let queue = self.queue.clone();
        let interval = self.cfg.cancel_poll;
        let shutdown = self.shutdown.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    biased;
                    () = shutdown.cancelled() => return,
                    () = cancel.cancelled() => return,
                    () = tokio::time::sleep(interval) => {},
                }
                match queue.statuses(receipt.trigger_ids()).await {
                    Ok(views) => {
                        if let Some(view) = views
                            .iter()
                            .find(|v| v.cancellation_requested || v.status.is_terminal())
                        {
                            debug!(patom.request.id = %view.request_id, "worker.cancel_watcher.fire");
                            cancel.cancel();
                            return;
                        }
                    }
                    Err(e) => warn!(error = %e, "worker.cancel_watcher.status.error"),
                }
            }
        })
    }
}

/// First (lead) trigger of a coalesced claim — the row whose sink mid-turn
/// chunks target and whose id the agent's appended artifacts carry.
fn lead_request_id(claim: &ClaimedTurn) -> PromptRequestId {
    *claim
        .trigger_ids
        .first()
        .expect("invariant: claim_next_turn drains at least one trigger")
}

/// Bridges `Agent` → `ResponseSink`: maps each `TurnObserver` event to a
/// [`ResponseChunk`] and fans it out to every trigger id in the current claim.
#[derive(Debug)]
struct FanOutObserver {
    sink: SharedResponseSink,
    user_id: UserId,
    ids: Vec<PromptRequestId>,
}

impl FanOutObserver {
    async fn fanout(&self, chunk: ResponseChunk) {
        for id in &self.ids {
            if let Err(e) = self
                .sink
                .publish_for_user(self.user_id, *id, chunk.clone())
                .await
            {
                debug!(error = %e, "fanout.publish.skipped");
            }
        }
    }
}

#[async_trait]
impl TurnObserver for FanOutObserver {
    async fn on_assistant(&self, content: &AssistantContent) {
        let chunk = match content {
            AssistantContent::Text(s) => ResponseChunk::Text { value: s.clone() },
            AssistantContent::Reasoning(s) => ResponseChunk::Reasoning { value: s.clone() },
            AssistantContent::ToolCall(c) => ResponseChunk::ToolCall(c.clone()),
        };
        self.fanout(chunk).await;
    }

    async fn on_tool_result(&self, result: &ToolResult) {
        self.fanout(ResponseChunk::ToolResult(result.clone())).await;
    }
}

// Worker-pool tests live in `tests/runtime_pipeline.rs` against real Postgres.
