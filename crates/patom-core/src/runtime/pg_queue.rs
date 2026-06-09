//! Postgres-backed [`PromptQueue`] + [`LeaseManager`].
//!
//! Backs the trait surface with four tables: `prompt_requests`, `session_leases`,
//! `session_turn_seq`, `sessions`. The agent loop, worker pool, hooks, and HTTP
//! handlers depend only on the traits, so this lives entirely behind that seam.
//!
//! All wall-clock values come from the injected [`SharedClock`] — never `NOW()` in
//! app SQL — so a `TestClock`-driven test sees lease-expiry boundaries firing on
//! cue (CLAUDE.md §11). Status enums and ids cross the SQL boundary via the
//! `sqlx::Type` impls in [`super::types`]; no hand-rolled string matching survives
//! here.

use std::fmt;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sqlx::PgPool;
use uuid::Uuid;

use crate::agents::AgentId;
use crate::auth::{OrgId, UserId, run_as_user, run_privileged};
use crate::clock::SharedClock;
use crate::colleagues::ColleagueId;
use crate::observability::propagation;
use crate::session::SessionId;
use crate::threads::ThreadId;
use crate::types::{Participant, Prompt};

use super::error::PromptError;
use super::limits::{MAX_ATTEMPTS, MAX_DAG_TURNS, MAX_PENDING_PER_SESSION};
use super::queue::{
    ClaimReceipt, ClaimedPrompt, ClaimedSession, ClaimedTurn, EnqueueOutcome, LeaseManager,
    LeaseTiming, LeaseToken, NewPromptRequest, NewTrigger, PromptQueue, RequestStatusView,
    TurnReceipt,
};
use super::types::{
    FailureReason, PromptRequestId, RequestKind, RequestKindPayload, RequestStatus, TurnSeq,
    WorkerId,
};

/// Postgres-backed queue + lease manager.
///
/// One type implements both traits because the claim-and-drain critical section
/// needs a single transaction across `prompt_requests` and `session_leases`.
pub struct PgPromptQueue {
    pool: PgPool,
    clock: SharedClock,
    timing: LeaseTiming,
    pending_cap: u32,
    max_attempts: u32,
}

impl PgPromptQueue {
    /// Construct with default caps from `runtime::limits`.
    #[must_use]
    pub fn new(pool: PgPool, clock: SharedClock) -> Self {
        Self::with_caps(
            pool,
            clock,
            LeaseTiming::default_const(),
            MAX_PENDING_PER_SESSION,
            MAX_ATTEMPTS,
        )
    }

    #[must_use]
    pub fn with_caps(
        pool: PgPool,
        clock: SharedClock,
        timing: LeaseTiming,
        pending_cap: u32,
        max_attempts: u32,
    ) -> Self {
        Self {
            pool,
            clock,
            timing,
            pending_cap,
            max_attempts,
        }
    }

    /// Lease timing — used by the worker pool to keep its heartbeat cadence in sync
    /// with the queue's TTL.
    #[must_use]
    pub const fn lease_timing(&self) -> LeaseTiming {
        self.timing
    }

    fn now(&self) -> DateTime<Utc> {
        self.clock.now_utc()
    }

    fn deadline(&self, now: DateTime<Utc>) -> DateTime<Utc> {
        now + chrono::Duration::from_std(self.timing.ttl())
            .expect("invariant: lease ttl fits in chrono::Duration")
    }

    /// Reset orphan rows — sessions whose lease expired before `now` are released, and
    /// any request stuck in `processing` for that session either returns to `pending`
    /// or, if it has already exhausted [`Self::max_attempts`], is parked as `failed`
    /// with `reason = poison`.
    ///
    /// Runs privileged (cross-tenant): the orphan scan crosses every
    /// org's lease table to reclaim work from crashed workers. The
    /// underlying tables are RLS-forced as of migration 18 — without
    /// `begin_privileged` the policy would filter to `app.user_id`'s
    /// org (unset here, so no rows match) and orphans would never be
    /// reclaimed.
    async fn reset_orphans(&self, now: DateTime<Utc>) -> Result<(), PromptError> {
        let mut tx = crate::auth::begin_privileged(&self.pool).await?;

        // Delete every expired lease in one shot, returning the (session, turn_seq)
        // pairs we need to fix up.
        let expired: Vec<(SessionId, TurnSeq)> = sqlx::query_as(
            "DELETE FROM session_leases
             WHERE leased_until <= $1
             RETURNING session_id, turn_seq",
        )
        .bind(now)
        .fetch_all(&mut *tx)
        .await?;

        if !expired.is_empty() {
            let max_attempts =
                i32::try_from(self.max_attempts).expect("invariant: max_attempts fits in i32");
            for (sid, stale_seq) in expired {
                // Combined update — for every processing row at the stale seq, either
                // park it as `failed` (attempts cap reached) or send it back to
                // `pending`. The failure reason binds typed via `FailureReason`'s
                // sqlx Encode (JSON-on-TEXT) so the Display form cannot drift into
                // storage by accident.
                sqlx::query(
                    "UPDATE prompt_requests
                     SET status = CASE WHEN attempts >= $1 THEN $2 ELSE $3 END,
                         failure_reason = CASE WHEN attempts >= $1 THEN $4 ELSE NULL END,
                         updated_at = $5
                     WHERE session_id = $6
                       AND status = $7
                       AND turn_seq = $8",
                )
                .bind(max_attempts)
                .bind(RequestStatus::Failed)
                .bind(RequestStatus::Pending)
                .bind(FailureReason::Poison)
                .bind(now)
                .bind(sid)
                .bind(RequestStatus::Processing)
                .bind(stale_seq)
                .execute(&mut *tx)
                .await?;
            }
        }

        tx.commit().await?;
        Ok(())
    }
}

impl fmt::Debug for PgPromptQueue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PgPromptQueue")
            .field("pending_cap", &self.pending_cap)
            .field("max_attempts", &self.max_attempts)
            .field("lease_ttl", &self.timing.ttl())
            .finish_non_exhaustive()
    }
}

// `clippy::too_many_lines` here counts the *entire* `#[async_trait]`-expanded
// impl block (every method's body inlined), not any one method. Each method
// is itself bounded; splitting the impl into multiple `impl` blocks would
// just hide the count without changing the code shape.
#[allow(clippy::too_many_lines)]
#[async_trait]
impl PromptQueue for PgPromptQueue {
    async fn enqueue(&self, req: NewPromptRequest) -> Result<EnqueueOutcome, PromptError> {
        run_privileged(&self.pool, async |tx| {
            enqueue_in_tx(self, tx.tx_mut(), req).await
        })
        .await
    }

    async fn enqueue_for_user(
        &self,
        acting_user_id: UserId,
        mut req: NewPromptRequest,
    ) -> Result<EnqueueOutcome, PromptError> {
        // Identity invariant: the persisted `created_by_user_id` is the
        // authenticated actor, not whatever the caller put in the
        // payload. Otherwise a same-org caller could enqueue a request
        // that stamps another member's id; every subsequent worker
        // `_for_user` write would then run under that other principal
        // (the `ClaimReceipt::acting_user_id` is derived from this
        // column). Overwriting here makes the spoof unrepresentable at
        // the storage layer.
        req.created_by_user_id = acting_user_id;
        run_as_user(&self.pool, acting_user_id, async |tx| {
            enqueue_in_tx(self, tx.tx_mut(), req).await
        })
        .await
    }

    async fn claim_next_session(
        &self,
        worker: WorkerId,
    ) -> Result<Option<ClaimedSession>, PromptError> {
        let now = self.now();
        let deadline = self.deadline(now);

        // Reset orphans in its own transaction first so the candidate scan sees an
        // up-to-date world. Cheap when there's nothing to reset.
        self.reset_orphans(now).await?;

        // Privileged: claim is cross-tenant by design — workers don't
        // know which org has work next. RLS would otherwise filter the
        // candidate scan to a single org and starve workers in others.
        let mut tx = crate::auth::begin_privileged(&self.pool).await?;

        let Some((session, session_org_id, session_user_id)) = next_candidate(&mut tx, now).await?
        else {
            tx.commit().await?;
            return Ok(None);
        };

        let next_seq = bump_turn_seq(&mut tx, session, session_org_id).await?;
        if !try_take_lease(
            &mut tx,
            session,
            session_org_id,
            worker,
            next_seq,
            deadline,
            now,
        )
        .await?
        {
            // Race-lost — another worker holds the live lease for this session.
            tx.commit().await?;
            return Ok(None);
        }

        let drained = drain_pending(&mut tx, session, next_seq, now).await?;

        if drained.is_empty() {
            tx.commit().await?;
            // All pending rows vanished between the candidate scan and the drain
            // (e.g. cancellation flipped them) — release the lease and report no work.
            let token = LeaseToken::build(session, worker, next_seq);
            let _ = self.release(&token).await;
            return Ok(None);
        }

        // §6: the receiver-is-agent trigger guarantees `agent_id` is `Some`
        // for any drained prompt; surface a backend error rather than panic
        // if it ever isn't.
        let receiver_agent_id = drained[0].2.ok_or_else(|| {
            PromptError::Backend(
                "drained prompt receiver colleague missing agent_id — \
                 receiver-is-agent trigger should make this unreachable"
                    .to_string(),
            )
        })?;
        let receiver_colleague_id =
            resolve_agent_colleague(&mut tx, session_org_id, receiver_agent_id).await?;
        tx.commit().await?;

        Ok(Some(build_claimed_session(
            session,
            session_org_id,
            session_user_id,
            receiver_colleague_id,
            worker,
            next_seq,
            drained,
        )?))
    }

    async fn mark_done(&self, receipt: &ClaimReceipt) -> Result<(), PromptError> {
        finalise(self, receipt, Finalisation::Done).await
    }

    async fn mark_failed(
        &self,
        receipt: &ClaimReceipt,
        reason: FailureReason,
    ) -> Result<(), PromptError> {
        finalise(self, receipt, Finalisation::Failed(reason)).await
    }

    async fn request_cancellation(&self, id: PromptRequestId) -> Result<(), PromptError> {
        let now = self.now();
        // Privileged: the HTTP route gates this call by opening a
        // `begin_as` tx, looking up the request to confirm visibility,
        // then dispatches here for the actual mutation — the same
        // pattern the agents / mcp_servers routes use. The store side
        // doesn't carry the principal, so we run privileged and trust
        // the caller's gate.
        let mut tx = crate::auth::begin_privileged(&self.pool).await?;
        let res = sqlx::query(
            "UPDATE prompt_requests
             SET cancellation_requested = TRUE, updated_at = $1
             WHERE id = $2",
        )
        .bind(now)
        .bind(id)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        if res.rows_affected() == 0 {
            return Err(PromptError::RequestNotFound(id));
        }
        Ok(())
    }

    async fn status(&self, id: PromptRequestId) -> Result<RequestStatusView, PromptError> {
        // Privileged: status is called from the worker's cancel watcher
        // (cross-tenant by construction — every worker polls every
        // claim it holds across orgs) and from HTTP cancel/stream
        // gates that have already verified the caller can see the
        // request. The store itself can't see the principal.
        let mut tx = crate::auth::begin_privileged(&self.pool).await?;
        let row: Option<(
            PromptRequestId,
            SessionId,
            RequestStatus,
            bool,
            Option<FailureReason>,
        )> = sqlx::query_as(
            "SELECT id, COALESCE(state_id, background_turn_id), status, cancellation_requested, \
                    failure_reason
             FROM prompt_requests
             WHERE id = $1",
        )
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?;
        tx.commit().await?;

        let Some((request_id, session, status, cancellation_requested, failure_reason)) = row
        else {
            return Err(PromptError::RequestNotFound(id));
        };
        Ok(RequestStatusView {
            request_id,
            session,
            status,
            cancellation_requested,
            failure_reason,
        })
    }

    /// Batch version: one round-trip via `WHERE id = ANY($1)`. A missing id
    /// is silently skipped (the cancel watcher and quiescence checks treat
    /// "row vanished" the same as "row not actionable"); callers that need
    /// strict NotFound can use the singular [`status`](Self::status).
    async fn statuses(
        &self,
        ids: &[PromptRequestId],
    ) -> Result<Vec<RequestStatusView>, PromptError> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        // Same rationale as `status`: cross-tenant infrastructure read.
        let mut tx = crate::auth::begin_privileged(&self.pool).await?;
        let rows: Vec<(
            PromptRequestId,
            SessionId,
            RequestStatus,
            bool,
            Option<FailureReason>,
        )> = sqlx::query_as(
            "SELECT id, COALESCE(state_id, background_turn_id), status, cancellation_requested, \
                    failure_reason
             FROM prompt_requests
             WHERE id = ANY($1)",
        )
        .bind(ids)
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;

        Ok(rows
            .into_iter()
            .map(
                |(request_id, session, status, cancellation_requested, failure_reason)| {
                    RequestStatusView {
                        request_id,
                        session,
                        status,
                        cancellation_requested,
                        failure_reason,
                    }
                },
            )
            .collect())
    }

    // Thread-feed trigger surface. The implementations live in the inherent
    // `impl PgPromptQueue` block below (alongside their private SQL helpers); the
    // fully-qualified calls expose them on the `dyn PromptQueue` surface without
    // ambiguity. Collapsed into single definitions in the P11 dead-code sweep.
    async fn enqueue_trigger(&self, trig: NewTrigger) -> Result<PromptRequestId, PromptError> {
        // `Self::` resolves to the inherent method (inherent items shadow trait
        // items of the same name), so this is delegation, not recursion.
        Self::enqueue_trigger(self, trig).await
    }

    async fn claim_next_turn(&self, worker: WorkerId) -> Result<Option<ClaimedTurn>, PromptError> {
        Self::claim_next_turn(self, worker).await
    }

    async fn mark_turn_done(&self, receipt: &TurnReceipt) -> Result<(), PromptError> {
        self.finalise_turn(receipt, RequestStatus::Done, None).await
    }

    async fn mark_turn_failed(
        &self,
        receipt: &TurnReceipt,
        reason: FailureReason,
    ) -> Result<(), PromptError> {
        self.finalise_turn(receipt, RequestStatus::Failed, Some(reason))
            .await
    }

    async fn heartbeat_turn(&self, receipt: &TurnReceipt) -> Result<(), PromptError> {
        self.heartbeat_turn_impl(receipt).await
    }

    async fn release_turn(&self, receipt: &TurnReceipt) -> Result<(), PromptError> {
        self.release_turn_impl(receipt).await
    }
}

#[async_trait]
impl LeaseManager for PgPromptQueue {
    async fn heartbeat(&self, lease: &LeaseToken) -> Result<(), PromptError> {
        let now = self.now();
        let deadline = self.deadline(now);
        // Privileged: lease management is cross-tenant infrastructure
        // (workers heartbeat every claim they hold across orgs). Same
        // reasoning as `reset_orphans`.
        let mut tx = crate::auth::begin_privileged(&self.pool).await?;
        let res = sqlx::query(
            "UPDATE session_leases
             SET leased_until = $1
             WHERE session_id = $2 AND worker_id = $3 AND turn_seq = $4",
        )
        .bind(deadline)
        .bind(lease.session())
        .bind(lease.worker())
        .bind(lease.turn_seq())
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        if res.rows_affected() == 0 {
            return Err(PromptError::LeaseStale {
                session: lease.session(),
            });
        }
        Ok(())
    }

    async fn release(&self, lease: &LeaseToken) -> Result<(), PromptError> {
        // Silent no-op if the lease has already moved on — the row count is not
        // checked, since release racing with orphan reclamation is benign.
        // Privileged for the same reason as `heartbeat`.
        let mut tx = crate::auth::begin_privileged(&self.pool).await?;
        sqlx::query(
            "DELETE FROM session_leases
             WHERE session_id = $1 AND worker_id = $2 AND turn_seq = $3",
        )
        .bind(lease.session())
        .bind(lease.worker())
        .bind(lease.turn_seq())
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }
}

/// Find the oldest pending request whose session has no live lease.
///
/// Two scoping rules per doc/memory.md §2.4:
///
/// 1. The session itself must have no live lease (existing rule).
/// 2. If the candidate row is non-normal (a memory-mutating
///    Reflection or Resolution job), the agent must not already
///    have *any* in-flight memory-mutating job. This serialises
///    reflection and resolution per agent so two of them cannot
///    race against the journal.
///
/// The partial index `prompt_requests_pending_idx (org_id,
/// session_id, created_at) WHERE status = 'pending'` is the
/// primary access path; the per-agent NOT EXISTS does a small
/// lookup against the live leases table.
///
/// The JOIN to `sessions` also carries the per-session tenancy
/// projection (`org_id`, `created_by_user_id`) back to the worker
/// so it can open a `begin_as_user` turn-tx. The queue itself runs
/// privileged because the scan is cross-tenant by construction.
async fn next_candidate(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    now: DateTime<Utc>,
) -> Result<Option<(SessionId, OrgId, UserId)>, PromptError> {
    let row = sqlx::query_as(
        "SELECT pr.session_id, s.org_id, s.created_by_user_id
         FROM prompt_requests pr
         JOIN sessions s ON s.id = pr.session_id
         WHERE pr.status = $1
           AND NOT EXISTS (
               SELECT 1 FROM session_leases sl
               WHERE sl.session_id = pr.session_id
                 AND sl.leased_until > $2
           )
           AND (
               pr.kind = $3
               OR NOT EXISTS (
                   SELECT 1 FROM prompt_requests pr2
                   JOIN session_leases sl2
                        ON sl2.session_id = pr2.session_id
                       AND sl2.leased_until > $2
                   WHERE pr2.receiver_colleague_id = pr.receiver_colleague_id
                     AND pr2.status = $4
                     AND pr2.kind <> $3
               )
           )
         ORDER BY pr.created_at ASC
         LIMIT 1",
    )
    .bind(RequestStatus::Pending)
    .bind(now)
    .bind(RequestKind::Normal)
    .bind(RequestStatus::Processing)
    .fetch_optional(&mut **tx)
    .await?;
    Ok(row)
}

/// Bump the per-session monotonic counter. The seq table uses
/// `INSERT ... ON CONFLICT` to stay race-free. `org_id` denormalisation
/// is enforced by the shared parity trigger (migration 16).
async fn bump_turn_seq(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    session: SessionId,
    org_id: OrgId,
) -> Result<TurnSeq, PromptError> {
    let (next_seq,): (TurnSeq,) = sqlx::query_as(
        "INSERT INTO session_turn_seq (session_id, org_id, next_seq)
         VALUES ($1, $2, 1)
         ON CONFLICT (session_id) DO UPDATE
             SET next_seq = session_turn_seq.next_seq + 1
         RETURNING next_seq",
    )
    .bind(session)
    .bind(org_id)
    .fetch_one(&mut **tx)
    .await?;
    Ok(next_seq)
}

/// Try to take the lease. If another worker beat us to it (their lease
/// is still live), the WHERE clause on the ON CONFLICT branch fails and
/// the statement returns 0 rows — that's the race-loss path; returns
/// `false`.
async fn try_take_lease(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    session: SessionId,
    org_id: OrgId,
    worker: WorkerId,
    next_seq: TurnSeq,
    deadline: DateTime<Utc>,
    now: DateTime<Utc>,
) -> Result<bool, PromptError> {
    let lease: Option<(SessionId,)> = sqlx::query_as(
        "INSERT INTO session_leases (session_id, org_id, worker_id, turn_seq, leased_until)
         VALUES ($1, $2, $3, $4, $5)
         ON CONFLICT (session_id) DO UPDATE
             SET worker_id = EXCLUDED.worker_id,
                 turn_seq = EXCLUDED.turn_seq,
                 leased_until = EXCLUDED.leased_until
             WHERE session_leases.leased_until <= $6
         RETURNING session_id",
    )
    .bind(session)
    .bind(org_id)
    .bind(worker)
    .bind(next_seq)
    .bind(deadline)
    .bind(now)
    .fetch_optional(&mut **tx)
    .await?;
    Ok(lease.is_some())
}

/// One row drained out of `prompt_requests` for a freshly-claimed session.
type DrainedPrompt = (
    PromptRequestId,
    String,
    // `agent_id` is joined from `colleagues.agent_id`, which is `NULL` for
    // the human kind. The receiver-is-agent trigger guarantees it's `Some`
    // for any prompt_requests row that ever ships through this path —
    // build_claimed_session asserts that invariant before driving the turn.
    Option<crate::agents::AgentId>,
    Option<String>,
    sqlx::types::Json<RequestKindPayload>,
);

/// Drain pending rows for the session: flip them to processing, stamp
/// the turn_seq, bump attempts. `receiver_agent_id`, `traceparent`, and
/// `kind_payload` are returned so the worker can resolve the right
/// Agent from the registry, attach its `handle_claim` span to the
/// producer's trace, and dispatch on job kind.
async fn drain_pending(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    session: SessionId,
    next_seq: TurnSeq,
    now: DateTime<Utc>,
) -> Result<Vec<DrainedPrompt>, PromptError> {
    // `receiver_agent_id` is joined out of the receiver colleague row — the
    // worker still drives dispatch by AgentId (the registry / WorkerPool key),
    // but the prompt_requests row itself only stores `receiver_colleague_id`.
    let drained = sqlx::query_as(
        "UPDATE prompt_requests pr
         SET status = $1,
             turn_seq = $2,
             attempts = attempts + 1,
             updated_at = $3
         FROM colleagues rc
         WHERE pr.session_id = $4 AND pr.status = $5
           AND rc.id = pr.receiver_colleague_id
         RETURNING pr.id, pr.content, rc.agent_id, pr.traceparent, pr.kind_payload",
    )
    .bind(RequestStatus::Processing)
    .bind(next_seq)
    .bind(now)
    .bind(session)
    .bind(RequestStatus::Pending)
    .fetch_all(&mut **tx)
    .await?;
    Ok(drained)
}

/// Assemble a [`ClaimedSession`] from a drained batch. Asserts the §6
/// invariants that every row in the batch targets the same receiver
/// and shares a kind, then parses each prompt body.
fn build_claimed_session(
    session: SessionId,
    org_id: OrgId,
    created_by_user_id: UserId,
    receiver_colleague_id: ColleagueId,
    worker: WorkerId,
    next_seq: TurnSeq,
    drained: Vec<DrainedPrompt>,
) -> Result<ClaimedSession, PromptError> {
    assert!(
        !drained.is_empty(),
        "invariant: caller checks `drained.is_empty()` before assembly"
    );
    // The receiver-is-agent trigger means `agent_id` is `Some` for any
    // queued prompt; `claim_next_session` already checked the head and
    // surfaced an error if it isn't.
    let receiver_agent_id = drained[0].2.expect(
        "invariant: receiver-is-agent trigger guarantees Some(agent_id) on the head of a claim",
    );
    let kind = drained[0].4.0.kind();
    for (_, _, rcv, _, p) in &drained[1..] {
        // A drained batch is one session's queued rows; the drain query
        // guarantees they share receiver + kind. Treat a violation as a
        // backend error (not a panic) for parity with the head-row guard —
        // the claim fails, the lease expires, another worker retries, rather
        // than unwinding the worker on a malformed batch.
        if *rcv != Some(receiver_agent_id) {
            return Err(PromptError::Backend(
                "drained prompts for one session must share receiver_agent_id".into(),
            ));
        }
        if p.0.kind() != kind {
            return Err(PromptError::Backend(
                "drained prompts for one session must share kind".into(),
            ));
        }
    }

    // Pick the first non-empty traceparent. A claim batch is the
    // worker's view of one logical turn — every prompt in it traces
    // back to the same producer span (the human POST or one
    // `send_message` call), so the heads agree.
    let traceparent = drained.iter().find_map(|(_, _, _, tp, _)| tp.clone());
    let kind_payload = drained[0].4.0.clone();

    let mut prompts = Vec::with_capacity(drained.len());
    for (request_id, content, _, _, _) in drained {
        let parsed = Prompt::try_from(content)?;
        prompts.push(ClaimedPrompt {
            request_id,
            content: parsed,
        });
    }

    Ok(ClaimedSession {
        session,
        org_id,
        created_by_user_id,
        receiver_agent_id,
        receiver_colleague_id,
        prompts,
        lease: LeaseToken::build(session, worker, next_seq),
        traceparent,
        kind_payload,
    })
}

/// Body of `enqueue` / `enqueue_for_user`. The runner owns
/// commit/rollback; the caller picks the scope (privileged for the
/// librarian/reflection/scheduler paths, tenant for HTTP / tool / worker
/// paths so the implicit-session-create + INSERT are RLS-checked).
async fn enqueue_in_tx(
    queue: &PgPromptQueue,
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    req: NewPromptRequest,
) -> Result<EnqueueOutcome, PromptError> {
    let now = queue.now();
    // Reflection / Resolution sit in `(Agent, System)` sessions so their
    // trace doesn't pollute the parent conversation; `receiver_agent_id`
    // still drives worker dispatch. For Normal we look up the receiver
    // agent's colleague_id inline — the trigger from migration 58 means
    // every agent has exactly one colleague row in its org.
    let kind = req.kind_payload.kind();
    let receiver = match kind {
        RequestKind::Normal => {
            let cid = resolve_agent_colleague(tx, req.org_id, req.receiver_agent_id).await?;
            Participant::agent(cid, req.receiver_agent_id)
        }
        RequestKind::Reflection | RequestKind::Resolution => Participant::System,
    };
    // §1: parse, don't validate. Normal sessions cannot host equal
    // participants; catch the violation before we hit Postgres.
    if kind == RequestKind::Normal && req.sender == receiver {
        return Err(PromptError::SelfSession);
    }

    if let Some(existing) = read_idempotent(tx, req.org_id, req.idempotency_key.as_str()).await? {
        return Ok(existing);
    }

    let request_id = PromptRequestId::new();
    let SessionResolution {
        session,
        root_request_id,
        is_new_session,
    } = resolve_session(tx, &req, receiver, request_id, now).await?;

    enforce_pending_cap(tx, session, queue.pending_cap).await?;
    insert_prompt_request(tx, &req, request_id, session, root_request_id, now).await?;
    if is_new_session {
        seed_dag_row(tx, root_request_id, req.org_id, now).await?;
    }

    Ok(EnqueueOutcome::Inserted {
        request_id,
        session,
        status: RequestStatus::Pending,
    })
}

/// Resolve the idempotency key for `(org_id, key)`. Returns the
/// existing `EnqueueOutcome::Existing` so callers can short-circuit.
async fn read_idempotent(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    org_id: OrgId,
    idempotency_key: &str,
) -> Result<Option<EnqueueOutcome>, PromptError> {
    let row: Option<(PromptRequestId, SessionId, RequestStatus)> = sqlx::query_as(
        "SELECT id, session_id, status FROM prompt_requests
         WHERE org_id = $1 AND idempotency_key = $2
         FOR UPDATE",
    )
    .bind(org_id)
    .bind(idempotency_key)
    .fetch_optional(&mut **tx)
    .await?;
    Ok(
        row.map(|(request_id, session, status)| EnqueueOutcome::Existing {
            request_id,
            session,
            status,
        }),
    )
}

/// Reject the enqueue if the session has hit its pending-row cap.
async fn enforce_pending_cap(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    session: SessionId,
    pending_cap: u32,
) -> Result<(), PromptError> {
    let pending_cap_i64 = i64::from(pending_cap);
    let (pending_count,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM prompt_requests
         WHERE session_id = $1 AND status = $2",
    )
    .bind(session)
    .bind(RequestStatus::Pending)
    .fetch_one(&mut **tx)
    .await?;
    if pending_count >= pending_cap_i64 {
        return Err(PromptError::PendingCapExceeded {
            session,
            max: pending_cap,
        });
    }
    Ok(())
}

/// Seed the DAG turn-budget row for a freshly-created root session.
async fn seed_dag_row(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    root_request_id: PromptRequestId,
    org_id: OrgId,
    now: DateTime<Utc>,
) -> Result<(), PromptError> {
    let cap = i64::from(MAX_DAG_TURNS);
    sqlx::query(
        "INSERT INTO prompt_request_dags
             (root_request_id, org_id, turns_used, turns_cap, created_at)
         VALUES ($1, $2, 0, $3, $4)",
    )
    .bind(root_request_id)
    .bind(org_id)
    .bind(cap)
    .bind(now)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// Insert a `prompt_requests` row inside the enqueue transaction. The
/// producer's W3C trace-context is captured here so the worker can stitch
/// its `handle_claim` span onto the same trace; a `None` return from
/// `current_traceparent` (exporter off, no active span) leaves the column
/// NULL and the worker starts a fresh root.
async fn insert_prompt_request(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    req: &NewPromptRequest,
    request_id: PromptRequestId,
    session: SessionId,
    root_request_id: PromptRequestId,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<(), PromptError> {
    let traceparent = propagation::current_traceparent();
    // The (kind, payload) pair is now true by construction: the kind is the
    // payload's variant discriminator, so no runtime cross-check is needed.
    let kind = req.kind_payload.kind();
    let payload_json = serde_json::to_value(&req.kind_payload)
        .expect("invariant: RequestKindPayload serialises infallibly via serde_json");

    // §6: the sender Participant is colleague-backed by construction (HTTP /
    // Slack / scheduler / librarian / reflection scheduler all resolve their
    // sender colleague before enqueue). Receiver colleague is resolved by
    // `enqueue_in_tx` upstream.
    let sender_colleague = req.sender.colleague_id().ok_or_else(|| {
        PromptError::Backend("enqueue: sender is System (humans/agents only)".to_string())
    })?;
    let receiver_colleague = resolve_agent_colleague(tx, req.org_id, req.receiver_agent_id).await?;

    sqlx::query(
        "INSERT INTO prompt_requests
             (id, session_id, org_id, content, idempotency_key, status,
              attempts, turn_seq, cancellation_requested, failure_reason,
              sender_colleague_id, receiver_colleague_id, root_request_id,
              traceparent,
              kind, kind_payload,
              created_at, updated_at)
         VALUES ($1, $2, $3, $4, $5, $6, 0, 0, FALSE, NULL,
                 $7, $8, $9,
                 $10,
                 $11, $12,
                 $13, $13)",
    )
    .bind(request_id)
    .bind(session)
    .bind(req.org_id)
    .bind(req.content.as_str())
    .bind(req.idempotency_key.as_str())
    .bind(RequestStatus::Pending)
    .bind(sender_colleague)
    .bind(receiver_colleague)
    .bind(root_request_id)
    .bind(traceparent)
    .bind(kind)
    .bind(payload_json)
    .bind(now)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// Result of `resolve_session` — the (session, dag root, is-new) triple
/// the enqueue path needs to populate the prompt_requests / dag rows.
#[derive(Debug)]
struct SessionResolution {
    session: SessionId,
    root_request_id: PromptRequestId,
    is_new_session: bool,
}

/// Resolve the session for an `enqueue`: either look up the existing one's
/// DAG root or mint a brand-new session row anchored at `request_id`.
async fn resolve_session(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    req: &NewPromptRequest,
    receiver: Participant,
    request_id: PromptRequestId,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<SessionResolution, PromptError> {
    if let Some(existing_session) = req.session {
        let row: Option<(PromptRequestId,)> =
            sqlx::query_as("SELECT root_request_id FROM sessions WHERE id = $1")
                .bind(existing_session)
                .fetch_optional(&mut **tx)
                .await?;
        let (root,) = row.ok_or(PromptError::SessionNotFound(existing_session))?;
        return Ok(SessionResolution {
            session: existing_session,
            root_request_id: root,
            is_new_session: false,
        });
    }
    let session_id = create_session_row(
        tx,
        request_id,
        req.sender,
        receiver,
        req.parent_session,
        req.org_id,
        req.created_by_user_id,
        now,
    )
    .await?;
    Ok(SessionResolution {
        session: session_id,
        root_request_id: request_id,
        is_new_session: true,
    })
}

/// Mint a session row for a fresh DAG, returning the session id.
///
/// The participant pair is canonicalised inside this helper so a caller that
/// passes `(sender, receiver)` either way round always produces the same row.
/// `root_request_id` is the about-to-be-inserted `prompt_requests.id`; the FK
/// from `prompt_requests.session_id` to `sessions.id` requires the session
/// row first, hence the explicit ordering. No FK is enforced from
/// `sessions.root_request_id` to `prompt_requests.id` so the
/// session-before-request order is legal.
#[allow(clippy::too_many_arguments)]
async fn create_session_row(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    root_request_id: PromptRequestId,
    sender: Participant,
    receiver: Participant,
    parent_session: Option<SessionId>,
    org_id: OrgId,
    created_by_user_id: UserId,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<SessionId, PromptError> {
    let (a, b) = Participant::canonical_pair(sender, receiver).ok_or(PromptError::SelfSession)?;
    // Slot `a` is always a real colleague (canonical pair sorts System last)
    // and the schema's NOT NULL on `participant_a_colleague_id` expresses the
    // same invariant. Slot `b` is NULL when paired with System (reflection /
    // resolution sessions).
    let a_colleague = a.colleague_id().ok_or_else(|| {
        PromptError::Backend(
            "canonical pair returned System in slot a — invariant violation".to_string(),
        )
    })?;
    let b_colleague = b.colleague_id();
    let session_id = SessionId::new();
    let res = sqlx::query(
        "INSERT INTO sessions
             (id, created_at, org_id, created_by_user_id,
              parent_session_id, root_request_id,
              participant_a_colleague_id, participant_b_colleague_id)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
    )
    .bind(session_id)
    .bind(now)
    .bind(org_id)
    .bind(created_by_user_id)
    .bind(parent_session)
    .bind(root_request_id)
    .bind(a_colleague)
    .bind(b_colleague)
    .execute(&mut **tx)
    .await;
    match res {
        Ok(_) => Ok(session_id),
        Err(sqlx::Error::Database(db)) if db.code().as_deref() == Some("23503") => {
            Err(PromptError::Backend(format!(
                "colleague_id FK violation creating session: {}",
                db.message(),
            )))
        }
        Err(e) => Err(e.into()),
    }
}

/// Outcome that drives [`finalise`]. Carries the (status, reason) pair as a
/// single value so callers cannot pass an inconsistent combination — the only
/// way to land in `failed` is to provide a [`FailureReason`].
#[derive(Debug)]
enum Finalisation {
    Done,
    Failed(FailureReason),
}

impl Finalisation {
    const fn status(&self) -> RequestStatus {
        match self {
            Self::Done => RequestStatus::Done,
            Self::Failed(_) => RequestStatus::Failed,
        }
    }

    fn reason(self) -> Option<FailureReason> {
        match self {
            Self::Done => None,
            Self::Failed(r) => Some(r),
        }
    }
}

/// Shared body of [`PgPromptQueue::mark_done`] / [`PgPromptQueue::mark_failed`]. Both
/// (a) verify the lease is still ours and (b) update every receipt id atomically.
async fn finalise(
    queue: &PgPromptQueue,
    receipt: &ClaimReceipt,
    outcome: Finalisation,
) -> Result<(), PromptError> {
    let now = queue.now();
    let lease = receipt.lease();
    let new_status = outcome.status();
    let failure_reason = outcome.reason();

    // Privileged: finalise runs from the worker post-turn — the worker
    // pool's tenancy plumbing is driven off `ClaimedSession.org_id` /
    // `created_by_user_id`, not the per-store tx. The lease fence
    // (`WHERE session_id = $1 AND worker_id = $2 AND turn_seq = $3`)
    // is the safety net against cross-claim writes.
    let mut tx = crate::auth::begin_privileged(&queue.pool).await?;

    let (lease_ok,): (bool,) = sqlx::query_as(
        "SELECT EXISTS(
            SELECT 1 FROM session_leases
            WHERE session_id = $1 AND worker_id = $2 AND turn_seq = $3
         )",
    )
    .bind(lease.session())
    .bind(lease.worker())
    .bind(lease.turn_seq())
    .fetch_one(&mut *tx)
    .await?;

    if !lease_ok {
        return Err(PromptError::LeaseStale {
            session: lease.session(),
        });
    }

    sqlx::query(
        "UPDATE prompt_requests
         SET status = $1,
             failure_reason = $2,
             updated_at = $3
         WHERE id = ANY($4) AND session_id = $5 AND turn_seq = $6",
    )
    .bind(new_status)
    .bind(failure_reason)
    .bind(now)
    .bind(receipt.ids())
    .bind(lease.session())
    .bind(lease.turn_seq())
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;
    Ok(())
}

/// Resolve the colleague_id for `agent` within `org`. Every agent's colleague
/// is minted by the trigger in migration 57, so the lookup either hits the
/// per-(org,agent) partial unique index or surfaces as a backend error
/// (a satellite without a colleague is a directory-integrity bug).
async fn resolve_agent_colleague(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    org: OrgId,
    agent: AgentId,
) -> Result<ColleagueId, PromptError> {
    let row: Option<(ColleagueId,)> =
        sqlx::query_as("SELECT id FROM colleagues WHERE org_id = $1 AND agent_id = $2")
            .bind(org)
            .bind(agent)
            .fetch_optional(&mut **tx)
            .await?;
    row.map(|(id,)| id).ok_or_else(|| {
        PromptError::Backend(format!(
            "no colleague mapped for agent {agent:?} in org {org:?}"
        ))
    })
}

// ─── Thread-feed trigger path (P2) ───────────────────────────────────────────
//
// The claim-key path coexists with the session path until the dead-code sweep.
// `claim_key = COALESCE(state_id, background_turn_id)` keys the lease + fence
// seq, so coalesce / serialise / re-address-follow-up transfer verbatim from
// `claim_next_session`, now per (thread, agent) instead of per session.

impl PgPromptQueue {
    /// Enqueue a thread-feed *trigger* (wake an agent for a turn). Idempotent on
    /// `(org_id, idempotency_key)`; the message itself already lives in
    /// `thread_messages`. Tenant-scoped on `acting_user_id`.
    pub async fn enqueue_trigger(&self, trig: NewTrigger) -> Result<PromptRequestId, PromptError> {
        let now = self.now();
        let id = PromptRequestId::new();
        // A root trigger (human @tag / scheduled fire) anchors the DAG on its own
        // id and seeds a budget row; an inherited trigger reuses the chain's root.
        let is_root_mint = trig.root_request_id.is_none();
        let root_request_id = trig.root_request_id.unwrap_or(id);
        let kind = trig.kind_payload.kind();
        let payload = serde_json::to_value(&trig.kind_payload)
            .expect("invariant: RequestKindPayload serialises infallibly via serde_json");
        let traceparent = propagation::current_traceparent();
        run_as_user::<PromptRequestId, PromptError>(&self.pool, trig.acting_user_id, async |tx| {
            // Receivers are agents (the prompt_requests_receiver_agent trigger
            // enforces it); resolve inside the same tenant tx.
            let receiver_colleague =
                resolve_agent_colleague(tx.tx_mut(), trig.org_id, trig.receiver_agent_id).await?;
            let inserted: Option<(PromptRequestId,)> = sqlx::query_as(
                "INSERT INTO prompt_requests
                     (id, org_id, content, idempotency_key, status, attempts, turn_seq,
                      cancellation_requested, failure_reason, sender_colleague_id,
                      receiver_colleague_id, root_request_id, traceparent, kind, kind_payload,
                      thread_id, state_id, background_turn_id, trigger_message_id, acting_user_id,
                      created_at, updated_at)
                 VALUES ($1, $2, NULL, $3, 'pending', 0, 0, FALSE, NULL, $4,
                         $5, $6, $7, $8, $9,
                         $10, $11, $12, $13, $14, $15, $15)
                 ON CONFLICT (org_id, idempotency_key) DO NOTHING
                 RETURNING id",
            )
            .bind(id)
            .bind(trig.org_id)
            .bind(trig.idempotency_key.as_str())
            .bind(trig.sender_colleague_id)
            .bind(receiver_colleague)
            .bind(root_request_id)
            .bind(traceparent)
            .bind(kind)
            .bind(&payload)
            .bind(trig.thread_id)
            .bind(trig.state_id)
            .bind(trig.background_turn_id)
            .bind(trig.trigger_message_id)
            .bind(trig.acting_user_id)
            .bind(now)
            .fetch_optional(&mut **tx.tx_mut())
            .await?;
            if let Some((rid,)) = inserted {
                // Seed the per-(human-tag/scheduled, agent) budget on a root mint.
                if is_root_mint {
                    let cap = i64::from(MAX_DAG_TURNS);
                    sqlx::query(
                        "INSERT INTO prompt_request_dags
                             (root_request_id, org_id, turns_used, turns_cap, created_at)
                         VALUES ($1, $2, 0, $3, $4)",
                    )
                    .bind(rid)
                    .bind(trig.org_id)
                    .bind(cap)
                    .bind(now)
                    .execute(&mut **tx.tx_mut())
                    .await?;
                }
                return Ok(rid);
            }
            // Idempotent retry — return the existing row's id.
            let (existing,): (PromptRequestId,) = sqlx::query_as(
                "SELECT id FROM prompt_requests WHERE org_id = $1 AND idempotency_key = $2",
            )
            .bind(trig.org_id)
            .bind(trig.idempotency_key.as_str())
            .fetch_one(&mut **tx.tx_mut())
            .await?;
            Ok(existing)
        })
        .await
    }

    /// Claim the next pending trigger, coalescing every pending trigger for one
    /// `claim_key` into a single [`ClaimedTurn`] and holding a per-`claim_key`
    /// lease (serialise). The (thread, agent) analogue of
    /// [`PromptQueue::claim_next_session`].
    pub async fn claim_next_turn(
        &self,
        worker: WorkerId,
    ) -> Result<Option<ClaimedTurn>, PromptError> {
        let now = self.now();
        let deadline = self.deadline(now);
        let mut tx = crate::auth::begin_privileged(&self.pool).await?;
        let Some((claim_key, org_id, acting_user_id)) = next_turn_candidate(&mut tx, now).await?
        else {
            tx.commit().await?;
            return Ok(None);
        };
        let lease_seq = bump_claim_seq(&mut tx, claim_key, org_id).await?;
        if !try_take_claim_lease(&mut tx, claim_key, org_id, worker, lease_seq, deadline, now)
            .await?
        {
            tx.commit().await?;
            return Ok(None);
        }
        let drained = drain_turn_pending(&mut tx, claim_key, lease_seq, now).await?;
        if drained.is_empty() {
            tx.commit().await?;
            return Ok(None);
        }
        tx.commit().await?;
        Ok(Some(build_claimed_turn(
            claim_key,
            org_id,
            acting_user_id,
            worker,
            lease_seq,
            drained,
        )?))
    }

    /// Mark every trigger in `receipt` as `Done`, fenced on the claim's
    /// `lease_seq` so a stale worker (whose lease was reclaimed) cannot finalise
    /// a turn another worker is now running. Privileged: the worker holds no
    /// per-request principal, and the receipt's id list is the authority. Flip
    /// the drained triggers to a terminal status (`Done` or `Failed` + reason),
    /// fenced on `lease_seq` so a reclaimed worker can't finalise another's turn.
    async fn finalise_turn(
        &self,
        receipt: &TurnReceipt,
        status: RequestStatus,
        reason: Option<FailureReason>,
    ) -> Result<(), PromptError> {
        let now = self.now();
        let mut tx = crate::auth::begin_privileged(&self.pool).await?;
        sqlx::query(
            "UPDATE prompt_requests
             SET status = $1, failure_reason = $2, updated_at = $3
             WHERE id = ANY($4) AND turn_seq = $5 AND status = $6",
        )
        .bind(status)
        .bind(reason)
        .bind(now)
        .bind(receipt.trigger_ids())
        .bind(receipt.lease_seq())
        .bind(RequestStatus::Processing)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Renew the `claim_key` lease, fenced on `lease_seq`.
    async fn heartbeat_turn_impl(&self, receipt: &TurnReceipt) -> Result<(), PromptError> {
        let now = self.now();
        let deadline = self.deadline(now);
        let mut tx = crate::auth::begin_privileged(&self.pool).await?;
        sqlx::query(
            "UPDATE claim_leases SET leased_until = $1 WHERE claim_key = $2 AND lease_seq = $3",
        )
        .bind(deadline)
        .bind(receipt.claim_key())
        .bind(receipt.lease_seq())
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Release the `claim_key` lease (delete the row), fenced on `lease_seq` so a
    /// stale worker cannot drop a lease another worker has taken over.
    async fn release_turn_impl(&self, receipt: &TurnReceipt) -> Result<(), PromptError> {
        let mut tx = crate::auth::begin_privileged(&self.pool).await?;
        sqlx::query("DELETE FROM claim_leases WHERE claim_key = $1 AND lease_seq = $2")
            .bind(receipt.claim_key())
            .bind(receipt.lease_seq())
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }
}

/// One drained trigger row:
/// `(id, kind, thread_id, receiver_colleague, agent_id, payload, root_request_id)`.
type DrainedTrigger = (
    PromptRequestId,
    RequestKind,
    Option<ThreadId>,
    ColleagueId,
    Option<AgentId>,
    sqlx::types::Json<RequestKindPayload>,
    PromptRequestId,
);

/// Oldest pending trigger whose `claim_key` has no live lease.
async fn next_turn_candidate(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    now: DateTime<Utc>,
) -> Result<Option<(Uuid, OrgId, UserId)>, PromptError> {
    let row: Option<(Uuid, OrgId, Option<UserId>)> = sqlx::query_as(
        "SELECT COALESCE(pr.state_id, pr.background_turn_id) AS claim_key, pr.org_id, pr.acting_user_id
         FROM prompt_requests pr
         WHERE pr.status = $1
           AND NOT EXISTS (
               SELECT 1 FROM claim_leases l
               WHERE l.claim_key = COALESCE(pr.state_id, pr.background_turn_id)
                 AND l.leased_until > $2)
         ORDER BY pr.created_at ASC
         LIMIT 1",
    )
    .bind(RequestStatus::Pending)
    .bind(now)
    .fetch_optional(&mut **tx)
    .await?;
    match row {
        Some((ck, org, Some(user))) => Ok(Some((ck, org, user))),
        Some((_, _, None)) => Err(PromptError::Backend(
            "trigger row missing acting_user_id — invariant violation".into(),
        )),
        None => Ok(None),
    }
}

/// Bump the per-`claim_key` lease-fence seq (decoupled from the feed seq).
async fn bump_claim_seq(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    claim_key: Uuid,
    org_id: OrgId,
) -> Result<TurnSeq, PromptError> {
    let (seq,): (TurnSeq,) = sqlx::query_as(
        "INSERT INTO claim_seq (claim_key, next_seq, org_id) VALUES ($1, 1, $2)
         ON CONFLICT (claim_key) DO UPDATE SET next_seq = claim_seq.next_seq + 1
         RETURNING next_seq",
    )
    .bind(claim_key)
    .bind(org_id)
    .fetch_one(&mut **tx)
    .await?;
    Ok(seq)
}

#[allow(clippy::too_many_arguments)]
async fn try_take_claim_lease(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    claim_key: Uuid,
    org_id: OrgId,
    worker: WorkerId,
    lease_seq: TurnSeq,
    deadline: DateTime<Utc>,
    now: DateTime<Utc>,
) -> Result<bool, PromptError> {
    let row: Option<(Uuid,)> = sqlx::query_as(
        "INSERT INTO claim_leases (claim_key, org_id, worker_id, lease_seq, leased_until)
         VALUES ($1, $2, $3, $4, $5)
         ON CONFLICT (claim_key) DO UPDATE
             SET worker_id = EXCLUDED.worker_id,
                 lease_seq = EXCLUDED.lease_seq,
                 leased_until = EXCLUDED.leased_until
             WHERE claim_leases.leased_until <= $6
         RETURNING claim_key",
    )
    .bind(claim_key)
    .bind(org_id)
    .bind(worker)
    .bind(lease_seq)
    .bind(deadline)
    .bind(now)
    .fetch_optional(&mut **tx)
    .await?;
    Ok(row.is_some())
}

/// Flip every pending trigger for `claim_key` to processing and return the batch.
async fn drain_turn_pending(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    claim_key: Uuid,
    lease_seq: TurnSeq,
    now: DateTime<Utc>,
) -> Result<Vec<DrainedTrigger>, PromptError> {
    let drained = sqlx::query_as(
        "UPDATE prompt_requests pr
         SET status = $1, turn_seq = $2, attempts = attempts + 1, updated_at = $3
         FROM colleagues rc
         WHERE COALESCE(pr.state_id, pr.background_turn_id) = $4 AND pr.status = $5
           AND rc.id = pr.receiver_colleague_id
         RETURNING pr.id, pr.kind, pr.thread_id, pr.receiver_colleague_id, rc.agent_id, \
                   pr.kind_payload, pr.root_request_id",
    )
    .bind(RequestStatus::Processing)
    .bind(lease_seq)
    .bind(now)
    .bind(claim_key)
    .bind(RequestStatus::Pending)
    .fetch_all(&mut **tx)
    .await?;
    Ok(drained)
}

/// Assemble a [`ClaimedTurn`], asserting the batch shares one receiver + kind.
fn build_claimed_turn(
    claim_key: Uuid,
    org_id: OrgId,
    acting_user_id: UserId,
    worker: WorkerId,
    lease_seq: TurnSeq,
    drained: Vec<DrainedTrigger>,
) -> Result<ClaimedTurn, PromptError> {
    assert!(
        !drained.is_empty(),
        "invariant: caller checks `drained.is_empty()` before assembly"
    );
    let receiver_agent_id = drained[0].4.ok_or_else(|| {
        PromptError::Backend(
            "drained trigger receiver missing agent_id — receiver-is-agent trigger \
             should make this unreachable"
                .into(),
        )
    })?;
    let kind = drained[0].1;
    let thread_id = drained[0].2;
    let receiver_colleague_id = drained[0].3;
    let kind_payload = drained[0].5.0.clone();
    let root_request_id = drained[0].6;
    for (_, k, _, _, rcv, _, _) in &drained[1..] {
        if *rcv != Some(receiver_agent_id) {
            return Err(PromptError::Backend(
                "drained triggers for one claim_key must share receiver_agent_id".into(),
            ));
        }
        if *k != kind {
            return Err(PromptError::Backend(
                "drained triggers for one claim_key must share kind".into(),
            ));
        }
    }
    let trigger_ids = drained.iter().map(|d| d.0).collect();
    Ok(ClaimedTurn {
        claim_key,
        kind,
        thread_id,
        org_id,
        acting_user_id,
        receiver_agent_id,
        receiver_colleague_id,
        trigger_ids,
        root_request_id,
        kind_payload,
        worker,
        lease_seq,
    })
}
