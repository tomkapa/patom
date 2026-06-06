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

use crate::auth::{OrgId, UserId, run_as_user, run_privileged};
use crate::clock::SharedClock;
use crate::observability::propagation;
use crate::session::SessionId;
use crate::types::{MessageSender, Participant, Prompt};

use super::error::PromptError;
use super::limits::{MAX_ATTEMPTS, MAX_DAG_TURNS, MAX_PENDING_PER_SESSION};
use super::queue::{
    ClaimReceipt, ClaimedPrompt, ClaimedSession, EnqueueOutcome, LeaseManager, LeaseTiming,
    LeaseToken, NewPromptRequest, PromptQueue, RequestStatusView,
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
        tx.commit().await?;

        if drained.is_empty() {
            // All pending rows vanished between the candidate scan and the drain
            // (e.g. cancellation flipped them) — release the lease and report no work.
            let token = LeaseToken::build(session, worker, next_seq);
            let _ = self.release(&token).await;
            return Ok(None);
        }

        Ok(Some(build_claimed_session(
            session,
            session_org_id,
            session_user_id,
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
            "SELECT id, session_id, status, cancellation_requested, failure_reason
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
            "SELECT id, session_id, status, cancellation_requested, failure_reason
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
                   WHERE pr2.receiver_agent_id = pr.receiver_agent_id
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
    crate::agents::AgentId,
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
    let drained = sqlx::query_as(
        "UPDATE prompt_requests
         SET status = $1,
             turn_seq = $2,
             attempts = attempts + 1,
             updated_at = $3
         WHERE session_id = $4 AND status = $5
         RETURNING id, content, receiver_agent_id, traceparent, kind_payload",
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
    worker: WorkerId,
    next_seq: TurnSeq,
    drained: Vec<DrainedPrompt>,
) -> Result<ClaimedSession, PromptError> {
    assert!(
        !drained.is_empty(),
        "invariant: caller checks `drained.is_empty()` before assembly"
    );
    let receiver_agent_id = drained[0].2;
    let kind = drained[0].4.0.kind();
    for (_, _, rcv, _, p) in &drained[1..] {
        assert_eq!(
            *rcv, receiver_agent_id,
            "invariant: drained prompts for one session must share receiver_agent_id"
        );
        assert_eq!(
            p.0.kind(),
            kind,
            "invariant: drained prompts for one session must share kind"
        );
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
    // still drives worker dispatch.
    let kind = req.kind_payload.kind();
    let receiver = match kind {
        RequestKind::Normal => Participant::agent(req.receiver_agent_id),
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

    sqlx::query(
        "INSERT INTO prompt_requests
             (id, session_id, org_id, content, idempotency_key, status,
              attempts, turn_seq, cancellation_requested, failure_reason,
              sender_kind, sender_agent_id,
              receiver_kind, receiver_agent_id, root_request_id,
              traceparent,
              kind, kind_payload,
              created_at, updated_at)
         VALUES ($1, $2, $3, $4, $5, $6, 0, 0, FALSE, NULL,
                 $7, $8, 'agent', $9, $10,
                 $11,
                 $12, $13,
                 $14, $14)",
    )
    .bind(request_id)
    .bind(session)
    .bind(req.org_id)
    .bind(req.content.as_str())
    .bind(req.idempotency_key.as_str())
    .bind(RequestStatus::Pending)
    .bind(MessageSender::from_participant(req.sender).kind())
    .bind(req.sender.agent_id())
    .bind(req.receiver_agent_id)
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
    let session_id = SessionId::new();
    let res = sqlx::query(
        "INSERT INTO sessions
             (id, created_at, org_id, created_by_user_id,
              parent_session_id, root_request_id,
              participant_a_kind, participant_a_agent_id,
              participant_b_kind, participant_b_agent_id)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)",
    )
    .bind(session_id)
    .bind(now)
    .bind(org_id)
    .bind(created_by_user_id)
    .bind(parent_session)
    .bind(root_request_id)
    .bind(a.kind())
    .bind(a.agent_id())
    .bind(b.kind())
    .bind(b.agent_id())
    .execute(&mut **tx)
    .await;
    match res {
        Ok(_) => Ok(session_id),
        Err(sqlx::Error::Database(db)) if db.code().as_deref() == Some("23503") => {
            Err(PromptError::Backend(format!(
                "agent_id FK violation creating session: {}",
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
