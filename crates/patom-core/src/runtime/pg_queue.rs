//! Postgres-backed [`PromptQueue`].
//!
//! Backs the trait surface with `prompt_requests`, `claim_leases`, and
//! `claim_seq`. The worker pool and HTTP handlers depend only on the trait, so
//! this lives entirely behind that seam.
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
use crate::auth::{OrgId, UserId, run_as_user};
use crate::clock::SharedClock;
use crate::colleagues::ColleagueId;
use crate::observability::propagation;
use crate::runtime::ClaimKey;
use crate::threads::ThreadId;

use super::error::PromptError;
use super::limits::{MAX_ATTEMPTS, MAX_DAG_TURNS, MAX_PENDING_PER_SESSION};
use super::queue::{
    ClaimedTurn, LeaseTiming, NewTrigger, PromptQueue, RequestStatusView, TurnReceipt,
};
use super::types::{
    FailureReason, PromptRequestId, RequestKind, RequestKindPayload, RequestStatus, TurnSeq,
    WorkerId,
};

/// Postgres-backed queue.
///
/// The claim-and-drain critical section runs in a single transaction across
/// `prompt_requests` and `claim_leases`.
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
            ClaimKey,
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

        let Some((request_id, claim_key, status, cancellation_requested, failure_reason)) = row
        else {
            return Err(PromptError::RequestNotFound(id));
        };
        Ok(RequestStatusView {
            request_id,
            claim_key,
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
            ClaimKey,
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
                |(request_id, claim_key, status, cancellation_requested, failure_reason)| {
                    RequestStatusView {
                        request_id,
                        claim_key,
                        status,
                        cancellation_requested,
                        failure_reason,
                    }
                },
            )
            .collect())
    }

    // Thread-feed trigger surface. The bodies live in the inherent
    // `impl PgPromptQueue` block below (alongside their private SQL helpers),
    // named `*_impl` so the trait method delegates without name ambiguity.
    async fn enqueue_trigger(&self, trig: NewTrigger) -> Result<PromptRequestId, PromptError> {
        self.enqueue_trigger_impl(trig).await
    }

    async fn claim_next_turn(&self, worker: WorkerId) -> Result<Option<ClaimedTurn>, PromptError> {
        self.claim_next_turn_impl(worker).await
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

// ─── Thread-feed trigger path ────────────────────────────────────────────────
//
// `claim_key = COALESCE(state_id, background_turn_id)` keys the lease + fence
// seq; the claim coalesces / serialises every pending trigger for one claim_key,
// per (thread, agent) for chat or per background turn for cognition.

impl PgPromptQueue {
    /// Enqueue a thread-feed *trigger* (wake an agent for a turn). Idempotent on
    /// `(org_id, idempotency_key)`; the message itself already lives in
    /// `thread_messages`. Tenant-scoped on `acting_user_id`. Private inherent
    /// helper behind the [`PromptQueue::enqueue_trigger`] trait method.
    async fn enqueue_trigger_impl(&self, trig: NewTrigger) -> Result<PromptRequestId, PromptError> {
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
    /// lease (serialise). Private inherent helper behind the
    /// [`PromptQueue::claim_next_turn`] trait method.
    async fn claim_next_turn_impl(
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
