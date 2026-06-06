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

use crate::agents::AgentId;
use crate::auth::{OrgId, UserId};
use crate::session::SessionId;
use crate::types::{Participant, Prompt};

use super::error::{LeaseTimingError, PromptError};
use super::limits::{LEASE_HEARTBEAT_INTERVAL, LEASE_TTL};
use super::types::{
    FailureReason, IdempotencyKey, PromptRequestId, RequestKindPayload, RequestStatus, TurnSeq,
    WorkerId,
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

/// Producer-side surface used by HTTP handlers.
#[async_trait]
pub trait PromptQueue: fmt::Debug + Send + Sync {
    async fn enqueue(&self, req: NewPromptRequest) -> Result<EnqueueOutcome, PromptError>;

    /// Tenant-scoped variant of [`Self::enqueue`]. Opens
    /// `begin_as_user(acting_user_id)` so the implicit-session
    /// create + the `prompt_requests` INSERT both run under the
    /// caller's principal. HTTP and tool callers thread the user id
    /// from their respective surfaces (request `Principal` /
    /// claimed session); the scheduler-side enqueue (librarian,
    /// reflection, scheduler) stays on the privileged entry point
    /// because it is cross-tenant by construction.
    async fn enqueue_for_user(
        &self,
        acting_user_id: UserId,
        req: NewPromptRequest,
    ) -> Result<EnqueueOutcome, PromptError>;
    async fn claim_next_session(
        &self,
        worker: WorkerId,
    ) -> Result<Option<ClaimedSession>, PromptError>;
    /// Mark every request in `receipt` as `Done`. The receipt binds the lease and the
    /// claimed ids together — there is no API for passing a foreign id list, so the
    /// "every id belongs to this lease's session" invariant is enforced by
    /// construction (and asserted again at the impl boundary).
    async fn mark_done(&self, receipt: &ClaimReceipt) -> Result<(), PromptError>;
    /// As [`mark_done`](Self::mark_done) but parks the rows as `Failed` with `reason`.
    async fn mark_failed(
        &self,
        receipt: &ClaimReceipt,
        reason: FailureReason,
    ) -> Result<(), PromptError>;
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

/// Lease-side surface — heartbeat and release. The token is opaque; only the impl
/// inspects its contents.
#[async_trait]
pub trait LeaseManager: fmt::Debug + Send + Sync {
    async fn heartbeat(&self, lease: &LeaseToken) -> Result<(), PromptError>;
    async fn release(&self, lease: &LeaseToken) -> Result<(), PromptError>;
}

/// Input to [`PromptQueue::enqueue`].
///
/// `session` is `Some` for follow-up prompts on an existing conversation;
/// `None` mints a fresh session bound to the canonical `(sender, Agent(receiver))`
/// pair and seeds a new DAG anchored at the resulting request id. Server-side
/// fields (`request_id`, `attempts`, `turn_seq`, `root_request_id`) are minted
/// by the queue, never carried in.
///
/// `sender` is currently always `Participant::Human` for HTTP entry but the
/// shape carries `Participant` so the future `send_message` tool can submit
/// agent-authored prompts through the same path.
#[derive(Debug, Clone)]
pub struct NewPromptRequest {
    /// Continuing an existing conversation. `None` ⇒ create a new session.
    pub session: Option<SessionId>,
    /// Author of the prompt.
    pub sender: Participant,
    /// Agent that should pick this up. Resolved on the HTTP boundary against
    /// the agents registry (defaults to the seeded default agent).
    pub receiver_agent_id: AgentId,
    /// Parent of the new session in the causal DAG. `None` for the root
    /// human-to-agent session; `Some` when an agent forks a sub-conversation.
    pub parent_session: Option<SessionId>,
    pub content: Prompt,
    pub idempotency_key: IdempotencyKey,
    /// Owning organisation. Required because every session this enqueue
    /// might mint is `NOT NULL` on `sessions.org_id` and the trigger
    /// rejects cross-org child sessions; HTTP callers pull this from the
    /// request principal, worker / tool callers pull it from the
    /// parent session's `tenancy`.
    pub org_id: OrgId,
    /// Human at the DAG root. Mirrors `sessions.created_by_user_id` so
    /// the worker pool can later set `app.user_id` from the claimed
    /// session and reads inside the turn run as that principal.
    pub created_by_user_id: UserId,
    /// Kind-specific payload — the job kind itself (doc/memory.md §2.1)
    /// is its variant discriminator, read via
    /// [`RequestKindPayload::kind`]. HTTP-triggered prompts default to
    /// [`RequestKindPayload::Normal`]; the reflection scheduler and
    /// librarian construct the `Reflection` / `Resolution` variants.
    /// Carrying the whole enum (not a separate `kind` scalar) makes
    /// `(kind, payload)` agreement true by construction — no runtime
    /// cross-check at the insert boundary.
    pub kind_payload: RequestKindPayload,
}

impl NewPromptRequest {
    /// Build a normal user-facing prompt — the shape every
    /// HTTP-triggered enqueue takes.
    #[must_use]
    #[allow(clippy::too_many_arguments)]
    pub fn normal(
        session: Option<SessionId>,
        sender: Participant,
        receiver_agent_id: AgentId,
        parent_session: Option<SessionId>,
        content: Prompt,
        idempotency_key: IdempotencyKey,
        org_id: OrgId,
        created_by_user_id: UserId,
    ) -> Self {
        Self {
            session,
            sender,
            receiver_agent_id,
            parent_session,
            content,
            idempotency_key,
            org_id,
            created_by_user_id,
            kind_payload: RequestKindPayload::Normal {},
        }
    }
}

/// Outcome of an enqueue. Idempotent retries return [`EnqueueOutcome::Existing`] so the
/// client gets the original request id back instead of a fresh one.
#[derive(Debug, Clone)]
pub enum EnqueueOutcome {
    /// A new row was inserted.
    Inserted {
        request_id: PromptRequestId,
        session: SessionId,
        status: RequestStatus,
    },
    /// The (idempotency_key) was already present; the original request id is returned
    /// verbatim along with its current status.
    Existing {
        request_id: PromptRequestId,
        session: SessionId,
        status: RequestStatus,
    },
}

impl EnqueueOutcome {
    #[must_use]
    pub const fn request_id(&self) -> PromptRequestId {
        match self {
            Self::Inserted { request_id, .. } | Self::Existing { request_id, .. } => *request_id,
        }
    }

    #[must_use]
    pub const fn session(&self) -> SessionId {
        match self {
            Self::Inserted { session, .. } | Self::Existing { session, .. } => *session,
        }
    }

    #[must_use]
    pub const fn status(&self) -> RequestStatus {
        match self {
            Self::Inserted { status, .. } | Self::Existing { status, .. } => *status,
        }
    }
}

/// Snapshot returned to a worker on a successful claim. The `prompts` are drained from
/// the queue under the same critical section as the lease so a second worker sees a
/// stable picture.
///
/// `receiver_agent_id` drives agent resolution at the worker — the worker
/// looks up the right `Agent` instance from a [`crate::agents::Agents`]
/// registry and runs the turn against it. All prompts in `prompts` share the
/// same receiver (every queue row carries the field; the drain assertion
/// confirms they agree).
///
/// `traceparent` is the W3C trace-context header the producer captured when
/// it enqueued the prompt; the worker uses it to attach its `handle_claim`
/// span to the producer's trace so an agent-chain conversation shows up as
/// one connected waterfall rather than N disconnected traces. `None` when
/// the producer had no active OTel context (exporter off, or the row was
/// enqueued before this column existed).
#[derive(Debug, Clone)]
pub struct ClaimedSession {
    pub session: SessionId,
    /// Owning organisation of the session. Joined from `sessions.org_id`
    /// during the claim so the worker can open a tenant-scoped turn-tx
    /// (`auth::begin_as_user`) without an extra round-trip per turn.
    pub org_id: OrgId,
    /// Human at the DAG root — `sessions.created_by_user_id`. Drives the
    /// `app.user_id` GUC inside the worker's turn-tx so RLS policies on
    /// reads from within the turn evaluate against the right principal.
    pub created_by_user_id: UserId,
    pub receiver_agent_id: AgentId,
    pub prompts: Vec<ClaimedPrompt>,
    pub lease: LeaseToken,
    pub traceparent: Option<String>,
    /// Kind-specific payload from the first drained row. The whole batch
    /// shares one variant because the queue groups by `(session, kind)`
    /// per claim, so this single value carries both the job kind (via
    /// [`RequestKindPayload::kind`]) and any per-variant metadata the
    /// worker needs for kind-specific post-turn dispatch (reflection
    /// checkpoint, no-action contradiction close). The `Normal` arm is
    /// a no-op today.
    pub kind_payload: RequestKindPayload,
}

impl ClaimedSession {
    /// Materialise a [`ClaimReceipt`] binding this claim's lease to the ids it
    /// drained. The worker carries the receipt through `mark_done` / `mark_failed`
    /// so those calls cannot be passed ids from a different session by accident.
    #[must_use]
    pub fn receipt(&self) -> ClaimReceipt {
        ClaimReceipt {
            lease: self.lease.clone(),
            ids: self.prompts.iter().map(|p| p.request_id).collect(),
            acting_user_id: self.created_by_user_id,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ClaimedPrompt {
    pub request_id: PromptRequestId,
    pub content: Prompt,
}

/// Proof that a worker holds a lease *and* the ids it drained under that lease.
///
/// The only handle accepted by [`PromptQueue::mark_done`] /
/// [`PromptQueue::mark_failed`]. Constructed solely via
/// [`ClaimedSession::receipt`]; both fields are private so a caller cannot forge
/// a receipt that mixes ids from another session with this lease (CLAUDE.md §1:
/// parse, don't validate).
#[derive(Debug, Clone)]
pub struct ClaimReceipt {
    lease: LeaseToken,
    ids: Vec<PromptRequestId>,
    /// Mirrors [`ClaimedSession::created_by_user_id`] so worker-side
    /// helpers that need to open a `begin_as_user` tx (publish a
    /// `Done` / `Error` chunk, mark the DAG row's quiescence) can
    /// reach the right principal through the receipt alone — the
    /// helper doesn't have to thread the claim through.
    acting_user_id: UserId,
}

impl ClaimReceipt {
    #[must_use]
    pub const fn lease(&self) -> &LeaseToken {
        &self.lease
    }

    #[must_use]
    pub fn ids(&self) -> &[PromptRequestId] {
        &self.ids
    }

    /// Human at the DAG root of the claimed session. Threaded into
    /// every `_for_user` store call the worker makes during finalise
    /// / quiescence / failure publish so the writes are RLS-checked.
    #[must_use]
    pub const fn acting_user_id(&self) -> UserId {
        self.acting_user_id
    }
}

/// Opaque proof of lease ownership. The worker carries this through every write so the
/// impl can fence stale operations with a `WHERE turn_seq = $token` style check.
#[derive(Debug, Clone)]
pub struct LeaseToken {
    session: SessionId,
    worker: WorkerId,
    turn_seq: TurnSeq,
}

impl LeaseToken {
    /// Build a token from the three values that identify a lease.
    ///
    /// `pub(crate)` because only queue impls inside this crate are allowed to mint
    /// tokens — external callers receive them via [`ClaimedSession`] and pass them
    /// back through [`PromptQueue`] / [`LeaseManager`] without ever constructing one.
    pub(crate) const fn build(session: SessionId, worker: WorkerId, turn_seq: TurnSeq) -> Self {
        Self {
            session,
            worker,
            turn_seq,
        }
    }

    #[must_use]
    pub const fn session(&self) -> SessionId {
        self.session
    }
    #[must_use]
    pub const fn worker(&self) -> WorkerId {
        self.worker
    }
    #[must_use]
    pub const fn turn_seq(&self) -> TurnSeq {
        self.turn_seq
    }
}

/// Cheap read-side view returned by [`PromptQueue::status`].
#[derive(Debug, Clone)]
pub struct RequestStatusView {
    pub request_id: PromptRequestId,
    pub session: SessionId,
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

/// Reference-counted lease-side handle held by workers.
pub type SharedLeaseManager = Arc<dyn LeaseManager>;
