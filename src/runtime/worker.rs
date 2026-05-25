//! Worker pool. A bounded `JoinSet` of N tasks, each running a claim-and-run loop:
//!
//! ```text
//! loop {
//!   match queue.claim_next_session(worker_id) {
//!     Some(claim) => {
//!       spawn heartbeat task on claim.lease (renew every TTL/3, dies on drop)
//!       result = timeout(MAX_TURN, agent.reply_batch(claim.session, prompts, cancel))
//!       publish chunks (Text, Done|Error) on the response sink
//!       mark_done | mark_failed
//!       release(lease)
//!     }
//!     None => sleep(1s)
//!   }
//! }
//! ```

use std::sync::Arc;
use std::time::Duration;

use sqlx::PgPool;
use tokio::task::{JoinHandle, JoinSet};
use tokio::time::timeout;
use tokio_util::sync::{CancellationToken, DropGuard};
use tracing::{Instrument, debug, info, warn};

use async_trait::async_trait;

use crate::agent_core::{Agent, AgentError, SharedTurnObserver, TurnObserver};
use crate::agents::SharedAgents;
use crate::observability::log::preview;
use crate::provider::{AssistantContent, ToolResult};
use crate::session::{SessionId, SharedSessionStore};
use crate::types::{AgentReply, Participant, Prompt};

use super::dag::SharedDagBudget;
use super::limits::{
    CANCEL_POLL_INTERVAL, MAX_PINGPONG_RETRIES, MAX_TURN_DURATION, MAX_WORKERS, WORKER_IDLE_POLL,
};
use super::queue::{
    ClaimReceipt, ClaimedSession, LeaseTiming, SharedLeaseManager, SharedPromptQueue,
};
use super::response::{ResponseChunk, SharedResponseSink};
use super::types::{FailureReason, PromptRequestId, RequestKind, RequestKindPayload, WorkerId};

/// System nudge appended to the receiver's history when the agent emitted a
/// turn without calling `send_message`.
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
    leases: SharedLeaseManager,
    sink: SharedResponseSink,
    agents: SharedAgents,
    sessions: SharedSessionStore,
    dag: SharedDagBudget,
    /// Direct pool handle used by the reflection dispatch path
    /// ([`Agent::reflect`] reads `session_messages` and writes to
    /// `reflection_checkpoints`). The normal-turn path goes through the
    /// trait surfaces and never touches this.
    pool: PgPool,
    /// Memory store handle. The resolution dispatch path uses it to close
    /// no-action contradictions (the mutation path closes inline via the
    /// memory tools).
    memory_store: crate::memory::SharedMemoryStore,
    /// Injected clock (CLAUDE.md §11). Used for any wall-clock reading
    /// the worker does directly — e.g. stamping reflection checkpoints.
    /// Production code never calls `chrono::Utc::now` directly.
    clock: crate::clock::SharedClock,
    cfg: WorkerConfig,
}

impl WorkerPool {
    #[allow(clippy::too_many_arguments)]
    #[must_use]
    pub fn new(
        queue: SharedPromptQueue,
        leases: SharedLeaseManager,
        sink: SharedResponseSink,
        agents: SharedAgents,
        sessions: SharedSessionStore,
        dag: SharedDagBudget,
        pool: PgPool,
        memory_store: crate::memory::SharedMemoryStore,
        clock: crate::clock::SharedClock,
        cfg: WorkerConfig,
    ) -> Self {
        Self {
            queue,
            leases,
            sink,
            agents,
            sessions,
            dag,
            pool,
            memory_store,
            clock,
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
                leases: self.leases.clone(),
                sink: self.sink.clone(),
                agents: self.agents.clone(),
                sessions: self.sessions.clone(),
                dag: self.dag.clone(),
                pool: self.pool.clone(),
                memory_store: self.memory_store.clone(),
                clock: self.clock.clone(),
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
    leases: SharedLeaseManager,
    sink: SharedResponseSink,
    agents: SharedAgents,
    sessions: SharedSessionStore,
    dag: SharedDagBudget,
    pool: PgPool,
    memory_store: crate::memory::SharedMemoryStore,
    clock: crate::clock::SharedClock,
    cfg: WorkerConfig,
    shutdown: CancellationToken,
}

impl Worker {
    /// Worker's main loop. Not wrapped in `#[instrument]` — the span would
    /// outlive the worker and orphan every per-claim child span. Letting
    /// `handle_claim` be the trace root keeps each prompt batch on one trace.
    async fn run(self) {
        loop {
            if self.shutdown.is_cancelled() {
                debug!(relay.worker.id = %self.id, "worker.shutdown");
                return;
            }
            match self.queue.claim_next_session(self.id).await {
                Ok(Some(claim)) => self.handle_claim(claim).await,
                Ok(None) => self.idle().await,
                Err(e) => {
                    warn!(relay.worker.id = %self.id, error = %e, "worker.claim.error");
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

    async fn handle_claim(&self, claim: ClaimedSession) {
        // Manual span — the `#[instrument]` macro doesn't emit OTel spans
        // for tasks spawned outside an open root.
        let span = tracing::info_span!(
            "worker.handle_claim",
            relay.worker.id = %self.id,
            relay.session.id = %claim.session,
            relay.agent.id = %claim.receiver_agent_id,
            relay.batch_size = claim.prompts.len(),
            relay.turn_seq = claim.lease.turn_seq().get(),
        );
        // Stitch onto the producer's trace so an agent-chain conversation
        // shows up as one connected waterfall.
        crate::observability::propagation::apply_parent(&span, claim.traceparent.as_deref());
        self.handle_claim_inner(claim).instrument(span).await;
    }

    async fn handle_claim_inner(&self, claim: ClaimedSession) {
        let prompts: Vec<Prompt> = claim.prompts.iter().map(|p| p.content.clone()).collect();
        let receipt = Arc::new(claim.receipt());
        let cancel = CancellationToken::new();

        if self.any_cancelled(receipt.ids()).await {
            self.publish_failure(&receipt, &FailureReason::Cancelled)
                .await;
            self.finalise(&receipt, FailureReason::Cancelled).await;
            // Without an explicit release the claim stays leased until the
            // TTL expires; under repeated cancels that stalls reclaim.
            if let Err(e) = self.leases.release(receipt.lease()).await {
                warn!(error = %e, "worker.lease.release.error");
            }
            return;
        }

        // Resolve the agent before spawning watchers so an unknown id fails
        // fast without holding a lease.
        let agent = match self.agents.get(claim.receiver_agent_id).await {
            Ok(a) => a,
            Err(e) => {
                warn!(
                    error = %e,
                    relay.agent.id = %claim.receiver_agent_id,
                    "worker.agent.resolve.error",
                );
                let reason = FailureReason::Unrecoverable(format!("agent resolve: {e}"));
                self.publish_failure(&receipt, &reason).await;
                self.finalise(&receipt, reason).await;
                if let Err(e) = self.leases.release(receipt.lease()).await {
                    warn!(error = %e, "worker.lease.release.error");
                }
                return;
            }
        };

        let heartbeat = self.spawn_heartbeat(receipt.clone());
        let cancel_watcher = self.spawn_cancel_watcher(receipt.clone(), cancel.clone());

        match claim.kind_payload.kind() {
            RequestKind::Normal => {
                let observer: SharedTurnObserver = Arc::new(FanOutObserver {
                    sink: self.sink.clone(),
                    receipt: receipt.clone(),
                });
                self.run_with_pingpong_guard(
                    &agent,
                    &claim,
                    prompts,
                    cancel.clone(),
                    observer,
                    &receipt,
                )
                .await;
            }
            RequestKind::Reflection | RequestKind::Resolution => {
                self.run_background_kind(&agent, &claim, prompts, cancel.clone(), &receipt)
                    .await;
            }
        }

        cancel_watcher.abort();
        let _ = cancel_watcher.await;
        heartbeat.abort();
        let _ = heartbeat.await;

        if let Err(e) = self.leases.release(receipt.lease()).await {
            warn!(error = %e, "worker.lease.release.error");
        }
    }

    /// Background kind dispatch (Reflection / Resolution).
    ///
    /// Routes through the same `agent.reply` path as a normal turn —
    /// differences: no observer (no SSE), no ping-pong guard (the model
    /// is free to end without a `send_message` call), and a single
    /// kind-specific post-turn step ([`Self::post_turn_for_kind`]) that
    /// matches on `claim.kind_payload`. Persists every LLM call into
    /// `session_messages` like normal turns so token-usage and
    /// behavioural traces are captured uniformly.
    async fn run_background_kind(
        &self,
        agent: &Agent,
        claim: &ClaimedSession,
        prompts: Vec<Prompt>,
        cancel: CancellationToken,
        receipt: &Arc<ClaimReceipt>,
    ) {
        let viewer = Participant::agent(claim.receiver_agent_id);
        let request_id = claim
            .prompts
            .first()
            .expect("invariant: claim drains at least one prompt")
            .request_id;

        let outcome = timeout(
            self.cfg.max_turn_duration,
            agent.reply(
                claim.session,
                viewer,
                prompts,
                request_id,
                crate::auth::Caller::new(claim.created_by_user_id, claim.org_id),
                claim.kind_payload.clone(),
                cancel,
                None,
            ),
        )
        .await;

        match outcome {
            Ok(Ok(reply)) => {
                if let Err(e) = self.queue.mark_done(receipt).await {
                    warn!(error = %e, "worker.background.mark_done.error");
                }
                self.post_turn_for_kind(claim, &reply).await;
                info!(
                    relay.session.id = %claim.session,
                    relay.agent.id = %claim.receiver_agent_id,
                    relay.request.kind = claim.kind_payload.kind().as_str(),
                    relay.send_message.calls = reply.send_message_calls(),
                    "worker.background.ok",
                );
            }
            Ok(Err(e)) => {
                warn!(error = %e, relay.request.kind = claim.kind_payload.kind().as_str(), "worker.background.error");
                self.finalise(receipt, FailureReason::Provider(e.to_string()))
                    .await;
            }
            Err(_elapsed) => {
                warn!(relay.session.id = %claim.session, relay.request.kind = claim.kind_payload.kind().as_str(), "worker.background.timeout");
                self.finalise(receipt, FailureReason::Timeout).await;
            }
        }
    }

    /// Single kind-specific post-turn dispatcher — every variant of
    /// [`RequestKindPayload`] gets its branch here. The exhaustive match
    /// makes "I forgot to handle the new kind" a compile error rather
    /// than a runtime no-op. Best-effort: every branch logs and proceeds
    /// rather than failing the request, since the turn itself already
    /// succeeded.
    async fn post_turn_for_kind(&self, claim: &ClaimedSession, reply: &AgentReply) {
        match &claim.kind_payload {
            RequestKindPayload::Normal {} => {
                // Nothing to do post-turn for normal claims; the success
                // path is just `mark_done` + DAG quiescence emission, both
                // handled by `run_with_pingpong_guard`. This arm exists so
                // the match stays exhaustive.
            }
            RequestKindPayload::Reflection {
                session_id: conversation_session,
                up_to_turn_id,
            } => {
                if let Err(e) = self
                    .write_reflection_checkpoint(
                        claim.receiver_agent_id,
                        *conversation_session,
                        *up_to_turn_id,
                        claim.session,
                        claim.created_by_user_id,
                    )
                    .await
                {
                    warn!(error = %e, "worker.reflect.checkpoint.error");
                }
            }
            RequestKindPayload::Resolution {
                contradiction_event_id,
            } => {
                self.close_no_action_if_unresolved(
                    *contradiction_event_id,
                    reply.final_text(),
                    claim.created_by_user_id,
                )
                .await;
            }
        }
    }

    /// If the resolution turn ended without a mutation tool closing the
    /// contradiction, stamp the row as a no-action close with the
    /// assistant's final text as the rationale. Best-effort — failure to
    /// close logs and proceeds (the librarian re-enqueues unresolved rows
    /// on the next sweep).
    async fn close_no_action_if_unresolved(
        &self,
        target: crate::memory::ContradictionEventId,
        final_text: &str,
        acting_user_id: crate::auth::UserId,
    ) {
        match self.memory_store.read_contradiction(target).await {
            Ok(Some(row)) if row.resolved_at.is_none() => {
                // The mutation path didn't close it — model chose no-action
                // implicitly. Truncate the final text to fit the column cap;
                // empty replies use a sentinel so we never violate the
                // 1..=N length invariant.
                let mut reason_raw = final_text.trim().to_string();
                if reason_raw.is_empty() {
                    reason_raw = "no-action (empty reply)".to_string();
                }
                if reason_raw.len() > crate::memory::CONTRADICTION_REASON_MAX_BYTES {
                    crate::tools::truncate_to_char_boundary(
                        &mut reason_raw,
                        crate::memory::CONTRADICTION_REASON_MAX_BYTES,
                    );
                }
                let reason = crate::memory::ResolutionReason::try_from(reason_raw)
                    .expect("invariant: 1..=cap enforced by trim+sentinel+truncate");
                if let Err(e) = self
                    .memory_store
                    .resolve_contradiction_for_user(
                        acting_user_id,
                        target,
                        crate::memory::ResolutionOutcome::NoAction { reason },
                    )
                    .await
                {
                    warn!(error = %e, relay.contradiction.id = %target, "worker.resolution.close.error");
                }
            }
            Ok(_) => {}
            Err(e) => {
                warn!(error = %e, relay.contradiction.id = %target, "worker.resolution.read.error");
            }
        }
    }

    /// Advance the checkpoint to `up_to_turn_id` (the slice the model saw)
    /// and append the just-finished reflection session id to the audit
    /// array. Using the payload cursor — not "latest now" — keeps the
    /// advance consistent with the slice and avoids skipping messages added
    /// between enqueue and completion.
    async fn write_reflection_checkpoint(
        &self,
        agent: crate::agents::AgentId,
        conversation_session: SessionId,
        up_to_turn_id: PromptRequestId,
        reflection_session: SessionId,
        acting_user_id: crate::auth::UserId,
    ) -> Result<(), sqlx::Error> {
        let now = self.clock.now_utc();
        // Tenant-scoped tx: `reflection_checkpoints` is RLS-forced
        // post-migration-17. `org_id` is derived from the parent agent
        // and parity-checked by `reflection_checkpoints_enforce_org`;
        // the `begin_as_user` here pins `app.user_id` to the same
        // principal the reflection claim was minted under so the
        // WITH CHECK fires against the right org.
        let mut tx = crate::auth::begin_as_user(&self.pool, acting_user_id).await?;
        sqlx::query(
            "INSERT INTO reflection_checkpoints
                 (agent_id, org_id, session_id, last_turn_id, reflection_event_id,
                  reflection_session_ids, created_at)
             VALUES ($1, (SELECT org_id FROM agents WHERE id = $1),
                     $2, $3, NULL, ARRAY[$4]::UUID[], $5)
             ON CONFLICT (agent_id, session_id) DO UPDATE
                 SET last_turn_id = EXCLUDED.last_turn_id,
                     reflection_event_id = EXCLUDED.reflection_event_id,
                     reflection_session_ids = array_append(
                         reflection_checkpoints.reflection_session_ids,
                         $4
                     ),
                     created_at = EXCLUDED.created_at",
        )
        .bind(agent)
        .bind(conversation_session)
        .bind(up_to_turn_id)
        .bind(reflection_session)
        .bind(now)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    async fn run_with_pingpong_guard(
        &self,
        agent: &Agent,
        claim: &ClaimedSession,
        prompts: Vec<Prompt>,
        cancel: CancellationToken,
        observer: SharedTurnObserver,
        receipt: &Arc<ClaimReceipt>,
    ) {
        // The session row alone can't disambiguate which side runs in an
        // agent↔agent session — `receiver_agent_id` is the queue's answer.
        let viewer = Participant::agent(claim.receiver_agent_id);
        // The drained batch's first request is the SSE sink mid-turn writes
        // (e.g. `send_message` AgentMessage chunks) target — its slot is open
        // for the duration of this claim, unlike the session's stored
        // `root_request_id` which can point at a long-quiesced sink in a
        // continuing thread.
        let request_id = claim
            .prompts
            .first()
            .expect("invariant: claim drains at least one prompt")
            .request_id;
        let mut retries: u8 = 0;
        let mut prompts_consumed = false;
        loop {
            let outcome = self
                .run_one_attempt(
                    agent,
                    claim.session,
                    viewer,
                    prompts.clone(),
                    request_id,
                    crate::auth::Caller::new(claim.created_by_user_id, claim.org_id),
                    cancel.clone(),
                    observer.clone(),
                    prompts_consumed,
                )
                .await;
            prompts_consumed = true;
            match outcome {
                Ok(Ok(reply)) if reply.send_message_calls() == 0 => {
                    if retries >= MAX_PINGPONG_RETRIES {
                        warn!(
                            relay.session.id = %claim.session,
                            relay.pingpong.retries = retries,
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
                        relay.session.id = %claim.session,
                        relay.pingpong.retries = retries,
                        text.preview = %preview(reply.final_text()),
                        "worker.turn.no_egress.retried",
                    );
                    if let Err(e) = self
                        .inject_pingpong_nudge(claim, request_id, claim.created_by_user_id)
                        .await
                    {
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
                    warn!(relay.session.id = %claim.session, "worker.turn.timeout");
                    self.publish_failure(receipt, &FailureReason::Timeout).await;
                    self.finalise(receipt, FailureReason::Timeout).await;
                    return;
                }
            }
        }
    }

    /// First attempt calls `reply` (appends the prompt); retries call
    /// `resume` so the prompt is not appended twice.
    #[allow(clippy::too_many_arguments)]
    async fn run_one_attempt(
        &self,
        agent: &Agent,
        session: SessionId,
        viewer: Participant,
        prompts: Vec<Prompt>,
        request_id: PromptRequestId,
        caller: crate::auth::Caller,
        cancel: CancellationToken,
        observer: SharedTurnObserver,
        prompts_consumed: bool,
    ) -> Result<Result<AgentReply, AgentError>, tokio::time::error::Elapsed> {
        if prompts_consumed {
            timeout(
                self.cfg.max_turn_duration,
                agent.resume(
                    session,
                    viewer,
                    request_id,
                    caller,
                    RequestKindPayload::Normal {},
                    cancel,
                    Some(observer),
                ),
            )
            .await
        } else {
            timeout(
                self.cfg.max_turn_duration,
                agent.reply(
                    session,
                    viewer,
                    prompts,
                    request_id,
                    caller,
                    RequestKindPayload::Normal {},
                    cancel,
                    Some(observer),
                ),
            )
            .await
        }
    }

    async fn inject_pingpong_nudge(
        &self,
        claim: &ClaimedSession,
        request_id: PromptRequestId,
        acting_user_id: crate::auth::UserId,
    ) -> Result<(), super::error::PromptError> {
        self.sessions
            .append_system_nudge_for_user(
                acting_user_id,
                claim.session,
                Participant::agent(claim.receiver_agent_id),
                PINGPONG_NUDGE.to_string(),
                request_id,
            )
            .await
            .map_err(|e| super::error::PromptError::Backend(format!("nudge: {e}")))
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

    async fn handle_success(&self, receipt: &ClaimReceipt, reply: AgentReply) {
        info!(
            relay.session.id = %receipt.lease().session(),
            bytes = reply.final_text().len(),
            relay.send_message.calls = reply.send_message_calls(),
            text.preview = %preview(reply.final_text()),
            "worker.turn.ok",
        );
        // No per-receipt `Done`: the terminal chunk fires only on DAG
        // quiescence so the SSE stream stays open while sibling agents work.
        if let Err(e) = self.queue.mark_done(receipt).await {
            warn!(error = %e, "worker.mark_done.error");
        }
        self.maybe_emit_quiescence(receipt).await;
    }

    async fn handle_agent_error(&self, receipt: &ClaimReceipt, err: AgentError) {
        // Exhaustive — a new `AgentError` variant must light up here rather
        // than silently falling through to `Provider`.
        let reason = match err {
            AgentError::Cancelled => FailureReason::Cancelled,
            AgentError::ProviderTimeout | AgentError::TodosLoadTimeout => FailureReason::Timeout,
            AgentError::HookDenied(d) => FailureReason::Hook(d.0),
            e @ (AgentError::Provider(_)
            | AgentError::Session(_)
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
            relay.session.id = %receipt.lease().session(),
            reason = reason.label(),
            detail = %reason,
            "worker.turn.error",
        );
        self.publish_failure(receipt, &reason).await;
        self.finalise(receipt, reason).await;
    }

    async fn publish_failure(&self, receipt: &ClaimReceipt, reason: &FailureReason) {
        let user_id = receipt.acting_user_id();
        for id in receipt.ids() {
            if let Err(e) = self
                .sink
                .publish_for_user(user_id, *id, ResponseChunk::from_failure(reason))
                .await
            {
                warn!(error = %e, "worker.publish.error.error");
            }
            if let Err(e) = self.sink.close_for_user(user_id, *id).await {
                warn!(error = %e, "worker.sink.close.error");
            }
        }
    }

    async fn finalise(&self, receipt: &ClaimReceipt, reason: FailureReason) {
        if let Err(e) = self.queue.mark_failed(receipt, reason).await {
            warn!(error = %e, "worker.mark_failed.error");
        }
        self.maybe_emit_quiescence(receipt).await;
    }

    /// Emit the terminal `Done` chunk on each claim request's sink when no
    /// `pending` / `processing` rows remain in this session's DAG.
    ///
    /// Quiescence is detected against the session's stored DAG root (the
    /// original first-prompt id). The `Done` chunk itself is published to the
    /// **claim's** request ids — those sinks are guaranteed open for the
    /// duration of the worker's claim, whereas the session's stored root may
    /// already be closed from a prior turn in a continuing thread. Postgres
    /// `LISTEN/NOTIFY` then routes the chunk by `prompt_requests.root_request_id`
    /// to the correct `/threads/{root}/stream` fan-in, so the user's UI sees
    /// the terminal chunk regardless of which prompt it was published on.
    async fn maybe_emit_quiescence(&self, receipt: &ClaimReceipt) {
        let session = receipt.lease().session();
        let user_id = receipt.acting_user_id();
        let root = match self.sessions.root_request_id(session).await {
            Ok(r) => r,
            Err(e) => {
                warn!(error = %e, relay.session.id = %session, "worker.quiescence.root_lookup.error");
                return;
            }
        };
        match self.dag.quiescent(root).await {
            Ok(true) => {
                for id in receipt.ids() {
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
                        // Synthetic ids that never opened a stream (test
                        // harness) surface as NotFound; benign no-op.
                        debug!(error = %e, relay.request.id = %id, "worker.quiescence.publish.skipped");
                        continue;
                    }
                    if let Err(e) = self.sink.close_for_user(user_id, *id).await {
                        debug!(error = %e, relay.request.id = %id, "worker.quiescence.close.skipped");
                    }
                }
                info!(relay.dag.root = %root, "worker.quiescence.done");
            }
            Ok(false) => {
                debug!(relay.dag.root = %root, "worker.quiescence.live");
            }
            Err(e) => {
                warn!(error = %e, relay.dag.root = %root, "worker.quiescence.query.error");
            }
        }
    }

    fn spawn_heartbeat(&self, receipt: Arc<ClaimReceipt>) -> JoinHandle<()> {
        let leases = self.leases.clone();
        let interval = self.cfg.lease_timing.heartbeat_interval();
        let shutdown = self.shutdown.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    biased;
                    () = shutdown.cancelled() => return,
                    () = tokio::time::sleep(interval) => {},
                }
                if let Err(e) = leases.heartbeat(receipt.lease()).await {
                    debug!(error = %e, "worker.heartbeat.stale");
                    return;
                }
            }
        })
    }

    /// Polls `queue.statuses` for every id in the receipt; the first one
    /// observed cancelled or terminal fires `cancel`. The agent honours it
    /// at its next checkpoint (between provider call and tool call). One
    /// round-trip per poll regardless of receipt size.
    fn spawn_cancel_watcher(
        &self,
        receipt: Arc<ClaimReceipt>,
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
                match queue.statuses(receipt.ids()).await {
                    Ok(views) => {
                        if let Some(view) = views
                            .iter()
                            .find(|v| v.cancellation_requested || v.status.is_terminal())
                        {
                            debug!(
                                relay.request.id = %view.request_id,
                                "worker.cancel_watcher.fire",
                            );
                            cancel.cancel();
                            return;
                        }
                    }
                    Err(e) => {
                        warn!(error = %e, "worker.cancel_watcher.status.error");
                    }
                }
            }
        })
    }
}

/// Bridges `Agent` → `ResponseSink`: maps each `TurnObserver` event to a
/// [`ResponseChunk`] and fans it out to every id in the current claim.
#[derive(Debug)]
struct FanOutObserver {
    sink: SharedResponseSink,
    receipt: Arc<ClaimReceipt>,
}

impl FanOutObserver {
    async fn fanout(&self, chunk: ResponseChunk) {
        let user_id = self.receipt.acting_user_id();
        for id in self.receipt.ids() {
            if let Err(e) = self
                .sink
                .publish_for_user(user_id, *id, chunk.clone())
                .await
            {
                warn!(error = %e, "fanout.publish.error");
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
