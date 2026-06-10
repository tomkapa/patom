//! Prompt queue and lease management — trait surface.
//!
//! Two traits ([`PromptQueue`] and [`LeaseManager`]) partition the surface so a HTTP
//! handler only sees the producer side and the worker only sees the consumer/lease
//! side. The Postgres impl in [`super::pg_queue`] is the only backend today; both
//! traits are intentionally async-trait-object-safe so future backends drop in.

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;

use uuid::Uuid;

use crate::agents::AgentId;
use crate::auth::{OrgId, UserId};
use crate::background::BackgroundTurnId;
use crate::colleagues::ColleagueId;
use crate::runtime::ClaimKey;
use crate::threads::{AgentThreadId, ThreadId, ThreadMessageId};

use super::error::{LeaseTimingError, PromptError};
use super::limits::{LEASE_HEARTBEAT_INTERVAL, LEASE_TTL};
use super::types::{
    FailureReason, IdempotencyKey, PromptRequestId, RequestKind, RequestKindPayload, RequestStatus,
    TurnSeq, WorkerId,
};

/// Co-validated lease timing.
///
/// Holds the lease TTL and the heartbeat cadence as one value so the queue and
/// the worker pool cannot drift apart at runtime: a queue's `lease_ttl` must
/// always exceed its worker's `heartbeat_interval`, otherwise the lease silently
/// dies between beats. Constructed via [`LeaseTiming::try_new`] which enforces
/// that invariant; both [`super::pg_queue::PgPromptQueue::with_caps`] and
/// [`super::WorkerConfig`] take a `LeaseTiming` so a single value seeds both sides.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LeaseTiming {
    ttl: Duration,
    heartbeat_interval: Duration,
}

impl LeaseTiming {
    /// Default timing from `runtime::limits` — `LEASE_TTL` / `LEASE_HEARTBEAT_INTERVAL`.
    /// The const-asserted ratio (`heartbeat * 3 == ttl`) holds at compile time, so the
    /// validation in [`Self::try_new`] is unconditional here — `expect` is a named
    /// assertion per CLAUDE.md §6.
    #[must_use]
    pub fn default_const() -> Self {
        Self::try_new(LEASE_TTL, LEASE_HEARTBEAT_INTERVAL)
            .expect("invariant: default LEASE_TTL/LEASE_HEARTBEAT_INTERVAL satisfy try_new")
    }

    /// Validate that `heartbeat_interval` is strictly less than `ttl` (so a missed
    /// beat still leaves time to recover) and non-zero.
    pub fn try_new(ttl: Duration, heartbeat_interval: Duration) -> Result<Self, LeaseTimingError> {
        if heartbeat_interval.is_zero() {
            return Err(LeaseTimingError::IntervalZero);
        }
        if heartbeat_interval >= ttl {
            return Err(LeaseTimingError::IntervalNotUnderTtl {
                ttl,
                heartbeat_interval,
            });
        }
        Ok(Self {
            ttl,
            heartbeat_interval,
        })
    }

    #[must_use]
    pub const fn ttl(&self) -> Duration {
        self.ttl
    }

    #[must_use]
    pub const fn heartbeat_interval(&self) -> Duration {
        self.heartbeat_interval
    }
}

impl Default for LeaseTiming {
    fn default() -> Self {
        Self::default_const()
    }
}

/// Producer-side surface used by HTTP handlers + the worker pool.
#[async_trait]
pub trait PromptQueue: fmt::Debug + Send + Sync {
    /// Enqueue a thread-feed *trigger* (wake an agent for a turn). Idempotent on
    /// `(org_id, idempotency_key)`; the message itself already lives in
    /// `thread_messages`. Tenant-scoped on the trigger's `acting_user_id`.
    async fn enqueue_trigger(&self, trig: NewTrigger) -> Result<PromptRequestId, PromptError>;
    /// Claim the next pending trigger, coalescing every pending trigger for one
    /// `claim_key` into a single [`ClaimedTurn`] under a per-`claim_key` lease.
    /// The thread-feed analogue of [`Self::claim_next_session`].
    async fn claim_next_turn(&self, worker: WorkerId) -> Result<Option<ClaimedTurn>, PromptError>;
    /// Mark every trigger in `receipt` as `Done` (fenced on the claim's
    /// `lease_seq`). The turn-path analogue of [`Self::mark_done`].
    async fn mark_turn_done(&self, receipt: &TurnReceipt) -> Result<(), PromptError>;
    /// As [`mark_turn_done`](Self::mark_turn_done) but parks the triggers as
    /// `Failed` with `reason`.
    async fn mark_turn_failed(
        &self,
        receipt: &TurnReceipt,
        reason: FailureReason,
    ) -> Result<(), PromptError>;
    /// Renew the `claim_key` lease (heartbeat), fenced on `lease_seq`.
    async fn heartbeat_turn(&self, receipt: &TurnReceipt) -> Result<(), PromptError>;
    /// Release the `claim_key` lease so the next pending trigger for it can be
    /// claimed without waiting for TTL expiry. Fenced on `lease_seq`.
    async fn release_turn(&self, receipt: &TurnReceipt) -> Result<(), PromptError>;
    async fn request_cancellation(&self, id: PromptRequestId) -> Result<(), PromptError>;
    /// Status accessor used by the SSE and cancel handlers — never required to be live;
    /// a snapshot is sufficient.
    async fn status(&self, id: PromptRequestId) -> Result<RequestStatusView, PromptError>;
    /// Batched [`status`](Self::status). The cancel watcher and quiescence
    /// checks need to inspect every id in a claim on every poll — running one
    /// round-trip per id turns into N×T queries per claim. The default here
    /// fans out to the singular method so impls that don't override stay
    /// correct; the Postgres impl overrides with a single `WHERE id = ANY`
    /// scan. Returned order is unspecified — match by `request_id`.
    async fn statuses(
        &self,
        ids: &[PromptRequestId],
    ) -> Result<Vec<RequestStatusView>, PromptError> {
        let mut out = Vec::with_capacity(ids.len());
        for id in ids {
            out.push(self.status(*id).await?);
        }
        Ok(out)
    }
}

/// Cheap read-side view returned by [`PromptQueue::status`].
#[derive(Debug, Clone)]
pub struct RequestStatusView {
    pub request_id: PromptRequestId,
    /// Polymorphic claim key — `COALESCE(state_id, background_turn_id)` from the
    /// status SQL: the chat participation id or the background turn id.
    pub claim_key: ClaimKey,
    pub status: RequestStatus,
    pub cancellation_requested: bool,
    /// Typed, never the raw column text. Decoded via [`FailureReason`]'s
    /// `sqlx::Decode` impl so callers cannot accidentally pattern-match on the
    /// `Display` form.
    pub failure_reason: Option<FailureReason>,
}

/// Reference-counted producer-side handle. The HTTP layer holds one and dispatches
/// dynamically through the trait.
pub type SharedPromptQueue = Arc<dyn PromptQueue>;

// ─── Thread-feed trigger model ───────────────────────────────────────────────
//
// In the thread-feed model a `prompt_requests` row is a *trigger* (wake agent X
// for a turn), not the message — the message lives in `thread_messages`. The
// claim/lease machinery is keyed onto a polymorphic `claim_key`:
//   claim_key = state_id (chat turn) OR background_turn_id (cognition turn).
// These types back the enqueue/claim path on
// [`super::pg_queue::PgPromptQueue`].

/// A wake-up trigger to enqueue.
///
/// Exactly one of `state_id` / `background_turn_id`
/// is `Some` (the `prompt_requests_claim_key_xor` CHECK enforces it). For a chat
/// trigger, `thread_id` + `state_id` are set and `trigger_message_id` points at
/// the feed message that caused the wake.
#[derive(Debug, Clone)]
pub struct NewTrigger {
    pub org_id: OrgId,
    /// Denormalised DAG-root human — drives the worker's RLS principal and
    /// keeps the claim a single join (no `sessions` lookup).
    pub acting_user_id: UserId,
    pub thread_id: Option<ThreadId>,
    pub state_id: Option<AgentThreadId>,
    pub background_turn_id: Option<BackgroundTurnId>,
    pub sender_colleague_id: ColleagueId,
    pub receiver_agent_id: AgentId,
    /// `None` => this trigger is a fresh DAG root (a human @tag or a scheduled
    /// fire): `enqueue_trigger` anchors the root on the new row's own id and
    /// seeds a `prompt_request_dags` budget row (`turns_cap = MAX_DAG_TURNS`).
    /// `Some(root)` => inherited along an agent→agent chain — no new budget.
    pub root_request_id: Option<PromptRequestId>,
    pub trigger_message_id: Option<ThreadMessageId>,
    pub idempotency_key: IdempotencyKey,
    pub kind_payload: RequestKindPayload,
}

/// A drained, leased batch of triggers for one `claim_key` — the thread-feed
/// analogue of [`ClaimedSession`].
///
/// `trigger_ids` are the coalesced wake rows
/// (one logical turn); the worker reads the thread feed at run time rather than
/// from any per-row `content`.
#[derive(Debug, Clone)]
pub struct ClaimedTurn {
    pub claim_key: Uuid,
    pub kind: RequestKind,
    pub thread_id: Option<ThreadId>,
    pub org_id: OrgId,
    pub acting_user_id: UserId,
    pub receiver_agent_id: AgentId,
    pub receiver_colleague_id: ColleagueId,
    pub trigger_ids: Vec<PromptRequestId>,
    /// DAG root of the coalesced triggers — the budget anchor the worker
    /// threads into the agent's tool context so `send_message` bumps the right
    /// `prompt_request_dags` row, and the key the worker checks for quiescence.
    pub root_request_id: PromptRequestId,
    pub kind_payload: RequestKindPayload,
    pub worker: WorkerId,
    pub lease_seq: TurnSeq,
}

impl ClaimedTurn {
    /// Materialise a [`TurnReceipt`] binding this claim's lease fence to the
    /// trigger ids it drained. Carried through `mark_turn_done` /
    /// `mark_turn_failed` / `release_turn` so those calls cannot be passed ids
    /// from another claim by accident.
    #[must_use]
    pub fn receipt(&self) -> TurnReceipt {
        TurnReceipt {
            claim_key: self.claim_key,
            lease_seq: self.lease_seq,
            trigger_ids: self.trigger_ids.clone(),
            acting_user_id: self.acting_user_id,
            root_request_id: self.root_request_id,
        }
    }
}

/// Proof that a worker holds a `claim_key` lease *and* the trigger ids it
/// drained under that fence.
///
/// The only handle accepted by the turn-finalise queue methods; constructed
/// solely via [`ClaimedTurn::receipt`] (private fields — a caller cannot forge
/// a receipt mixing another claim's ids).
#[derive(Debug, Clone)]
pub struct TurnReceipt {
    claim_key: Uuid,
    lease_seq: TurnSeq,
    trigger_ids: Vec<PromptRequestId>,
    acting_user_id: UserId,
    root_request_id: PromptRequestId,
}

impl TurnReceipt {
    #[must_use]
    pub const fn claim_key(&self) -> Uuid {
        self.claim_key
    }
    #[must_use]
    pub const fn lease_seq(&self) -> TurnSeq {
        self.lease_seq
    }
    #[must_use]
    pub fn trigger_ids(&self) -> &[PromptRequestId] {
        &self.trigger_ids
    }
    #[must_use]
    pub const fn acting_user_id(&self) -> UserId {
        self.acting_user_id
    }
    #[must_use]
    pub const fn root_request_id(&self) -> PromptRequestId {
        self.root_request_id
    }
}
