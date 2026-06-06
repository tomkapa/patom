//! Storage seam for the memory subsystem (doc/memory.md §1.2, §1.7–§1.9).
//!
//! One transactional mutation function ([`MemoryStore::apply`]) is the only
//! path through which memory changes — agent tool calls, operator notes, and
//! librarian sweeps all funnel through it. Every mutation appends one row to
//! the journal ([`MemoryEvent`]) and updates the materialized table
//! ([`MemoryRow`]) in the same transaction.
//!
//! The trait keeps the worker pool, librarian, and HTTP layer decoupled from
//! Postgres so the rest of the runtime can be exercised against an in-memory
//! fake when integration tests do not want a database.

use std::fmt;
use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use thiserror::Error;

use crate::agents::AgentId;
use crate::auth::{OrgId, UserId};
use crate::provider::ProviderError;
use crate::runtime::PromptRequestId;
use crate::types::ParseError;

use super::limits::{CONTRADICTION_REASON_MAX_BYTES, VALIDATION_EVIDENCE_MAX_BYTES};
use super::types::{
    ContradictionEventId, MemoryContent, MemoryEventId, MemoryId, MemoryKind, MemoryState,
    MutationKind, MutationSourceKind,
};

/// All failure modes the memory store can surface. CLAUDE.md §12 — one error
/// type at the module boundary, every variant exhaustive at the call site.
#[derive(Debug, Error)]
pub enum MemoryStoreError {
    /// Target memory id was not found in `agent_memories`. Update / forget
    /// hit a row that has already been forgotten or never existed.
    #[error("memory {id:?} not found")]
    NotFound { id: MemoryId },

    /// Target journal event id was not found in `memory_events`. Surfaced
    /// by the revert path against a stale id.
    #[error("memory event {id:?} not found")]
    EventNotFound { id: MemoryEventId },

    /// Resolve-contradiction touched zero rows. Either the row was
    /// already resolved, the id is unknown, or RLS filtered it out as
    /// belonging to another tenant. Treating zero-affected as success
    /// would silently lie to the librarian / resolution turn.
    #[error("contradiction {0:?} not found or already resolved")]
    ContradictionNotFound(ContradictionEventId),

    /// Target row exists but belongs to a different agent. Memory is
    /// per-agent and private (doc/memory.md §1.11) — a cross-agent edit is
    /// a programming bug, not a recoverable error.
    #[error("memory {id:?} does not belong to agent {agent:?}")]
    WrongAgent { id: MemoryId, agent: AgentId },

    /// Pinned-row protection. Agent-driven mutations cannot edit or forget
    /// pinned rows; the operator path bypasses this check.
    #[error("memory {id:?} is pinned and cannot be mutated by an agent")]
    PinnedImmutable { id: MemoryId },

    /// Boundary parsing failure for a derived input (content cap exceeded
    /// during a rebuild, malformed handle).
    #[error("parse: {0}")]
    Parse(#[from] ParseError),

    /// Underlying Postgres error — wrapped as opaque so callers do not
    /// pattern-match on driver internals.
    #[error("memory store db error: {0}")]
    Db(#[from] sqlx::Error),

    /// Embedding provider call failed during a mutation. `MemoryStore::apply`
    /// propagates this so a missing embedding never produces a row that
    /// retrieval cannot match.
    #[error("memory store embedding provider: {0}")]
    Provider(#[from] ProviderError),
}

/// Provenance attached to every mutation. `Turn` rides on the calling
/// prompt-request id (the agent that just spoke); `Operator` and
/// `Librarian` carry no detail.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MutationSource {
    Turn(PromptRequestId),
    Operator,
    Librarian,
}

impl MutationSource {
    /// The kind label persisted on the journal row.
    #[must_use]
    pub const fn kind(&self) -> MutationSourceKind {
        match self {
            Self::Turn(_) => MutationSourceKind::Turn,
            Self::Operator => MutationSourceKind::Operator,
            Self::Librarian => MutationSourceKind::Librarian,
        }
    }

    /// Detail column — only `Turn` carries a request id.
    #[must_use]
    pub const fn turn_id(&self) -> Option<PromptRequestId> {
        match self {
            Self::Turn(id) => Some(*id),
            Self::Operator | Self::Librarian => None,
        }
    }

    /// Only the operator path is allowed to mutate pinned rows. Derived
    /// from the source so the predicate cannot drift out of sync with a
    /// separate `operator_override` boolean.
    #[must_use]
    pub const fn bypasses_pin(&self) -> bool {
        matches!(self, Self::Operator)
    }
}

/// Input to [`MemoryStore::apply`].
///
/// The variant selects which journal label is written and which
/// materialized-row update runs; pinned-row protection is derived from the
/// source (`source.bypasses_pin()`), not a parallel boolean. Agent tool
/// calls pass `MutationSource::Turn(_)`; the HTTP operator surface passes
/// `Operator`; the librarian passes `Librarian`.
#[derive(Debug, Clone)]
pub enum MemoryMutation {
    /// Mint a new memory row.
    Write {
        agent: AgentId,
        kind: MemoryKind,
        content: MemoryContent,
        state: MemoryState,
        pinned: bool,
        source: MutationSource,
    },
    /// Replace `content` on an existing memory. `state` is the *new* state
    /// after the update; kind and pinned are preserved from the prior row.
    Update {
        agent: AgentId,
        target: MemoryId,
        content: MemoryContent,
        state: MemoryState,
        source: MutationSource,
    },
    /// Drop a memory row from the materialized view. The journal retains
    /// the event so the row can be replayed back into existence.
    Forget {
        agent: AgentId,
        target: MemoryId,
        source: MutationSource,
    },
}

impl MemoryMutation {
    /// Mutation kind — drives the journal `mutation` column.
    #[must_use]
    pub const fn mutation_kind(&self) -> MutationKind {
        match self {
            Self::Write { .. } => MutationKind::Write,
            Self::Update { .. } => MutationKind::Update,
            Self::Forget { .. } => MutationKind::Forget,
        }
    }

    /// Owning agent — the row's `agent_id`. The store cross-checks against
    /// the materialized row on update / forget so a misrouted call surfaces
    /// as [`MemoryStoreError::WrongAgent`] rather than corrupting state.
    #[must_use]
    pub const fn agent(&self) -> AgentId {
        match self {
            Self::Write { agent, .. } | Self::Update { agent, .. } | Self::Forget { agent, .. } => {
                *agent
            }
        }
    }

    /// Provenance attached to the journal row.
    #[must_use]
    pub const fn source(&self) -> MutationSource {
        match self {
            Self::Write { source, .. }
            | Self::Update { source, .. }
            | Self::Forget { source, .. } => *source,
        }
    }
}

/// Outcome of a successful [`MemoryStore::apply`] call. The materialized
/// `row` is `None` on `Forget` (the row is gone) and `Some` on
/// `Write` / `Update`.
#[derive(Debug, Clone)]
pub struct MutationOutcome {
    pub event_id: MemoryEventId,
    pub memory_id: MemoryId,
    pub row: Option<MemoryRow>,
}

/// Snapshot of a single row in `agent_memories`.
///
/// `source_turn_id` is `None` for operator notes and librarian-merged rows
/// (neither has a producing turn); `Some(_)` for agent-written rows.
#[derive(Debug, Clone)]
pub struct MemoryRow {
    pub id: MemoryId,
    pub agent_id: AgentId,
    /// Owning org of the parent agent, denormalised onto every row by
    /// the tenancy retrofit (migration 17). RLS keys on it directly.
    pub org_id: OrgId,
    pub kind: MemoryKind,
    pub content: MemoryContent,
    pub state: MemoryState,
    pub pinned: bool,
    pub source_turn_id: Option<PromptRequestId>,
    pub created_at: DateTime<Utc>,
    pub last_validated_at: DateTime<Utc>,
    pub last_accessed_at: DateTime<Utc>,
    pub access_count: u64,
}

/// Per-mutation payload journaled with every event.
///
/// The variant matches the `memory_events_content_shape` +
/// `memory_events_payload_shape` CHECK constraints exactly: every Rust
/// shape is a valid row, every row decodes into exactly one shape. Replay
/// and revert match the variant without touching an `Option`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MemoryEventPayload {
    /// New row minted. Carries the full attrs needed to recreate it.
    Write {
        content: MemoryContent,
        kind: MemoryKind,
        state: MemoryState,
        pinned: bool,
    },
    /// Content (and possibly state) changed. `kind` and `pinned` are the
    /// post-mutation values; today an Update does not change either, but
    /// recording them keeps replay self-contained.
    Update {
        before: MemoryContent,
        after: MemoryContent,
        kind: MemoryKind,
        state: MemoryState,
        pinned: bool,
    },
    /// Row removed from the materialized view. `before` is the content at
    /// the moment of removal, so a revert can restore it without consulting
    /// the prior write event.
    Forget { before: MemoryContent },
}

impl MemoryEventPayload {
    /// Discriminator — drives the `memory_events.mutation` column.
    #[must_use]
    pub const fn kind(&self) -> MutationKind {
        match self {
            Self::Write { .. } => MutationKind::Write,
            Self::Update { .. } => MutationKind::Update,
            Self::Forget { .. } => MutationKind::Forget,
        }
    }
}

/// Snapshot of a single row in `memory_events`.
#[derive(Debug, Clone)]
pub struct MemoryEvent {
    pub id: MemoryEventId,
    pub agent_id: AgentId,
    /// Owning org of the parent agent, denormalised onto every event by
    /// the tenancy retrofit (migration 17).
    pub org_id: OrgId,
    pub target_memory_id: MemoryId,
    pub source: MutationSource,
    pub created_at: DateTime<Utc>,
    pub payload: MemoryEventPayload,
}

impl MemoryEvent {
    /// Discriminator — drives the journal `mutation` column.
    #[must_use]
    pub const fn mutation_kind(&self) -> MutationKind {
        self.payload.kind()
    }
}

/// Filter applied by [`MemoryStore::search_by_embedding`].
#[derive(Debug, Clone, Default)]
pub struct SearchFilter {
    pub kinds: Option<Vec<crate::memory::MemoryKind>>,
    pub min_state: Option<crate::memory::MemoryState>,
}

/// One result from an embedding search — the row plus its cosine similarity
/// score in `[-1, 1]` (higher = closer).
#[derive(Debug, Clone)]
pub struct ScoredMemoryRow {
    pub row: MemoryRow,
    pub similarity: f32,
}

/// A pair of memories the librarian flagged as duplicates / contradicting.
#[derive(Debug, Clone)]
pub struct PairCandidate {
    pub a: MemoryRow,
    pub b: MemoryRow,
    pub similarity: f32,
}

/// Storage seam for the memory subsystem. Implementations must be
/// thread-safe.
///
/// The trait is intentionally narrow: every mutation flows through
/// [`Self::apply`]; everything else is read or replay.
#[async_trait]
pub trait MemoryStore: fmt::Debug + Send + Sync {
    /// Apply one mutation. Appends a journal row and updates the
    /// materialized table in a single transaction.
    async fn apply(&self, mutation: MemoryMutation) -> Result<MutationOutcome, MemoryStoreError>;

    /// Tenant-scoped variant of [`Self::apply`]. Opens
    /// `begin_as_user(acting_user_id)` so the journal +
    /// materialized-row INSERT/UPDATE/DELETE run under the acting
    /// principal's RLS context — a worker or tool acting on behalf
    /// of a foreign-org user is rejected at the WITH CHECK boundary
    /// on `agent_memories` / `memory_events`. The librarian's
    /// cross-tenant sweeps stay on the privileged entry point.
    async fn apply_for_user(
        &self,
        acting_user_id: UserId,
        mutation: MemoryMutation,
    ) -> Result<MutationOutcome, MemoryStoreError>;

    /// Snapshot every materialized row for `agent`, ordered by `created_at`
    /// ascending.
    async fn list(&self, agent: AgentId) -> Result<Vec<MemoryRow>, MemoryStoreError>;

    /// Fetch a single materialized row. Returns `None` if forgotten or
    /// never existed.
    async fn get(&self, id: MemoryId) -> Result<Option<MemoryRow>, MemoryStoreError>;

    /// Snapshot every journal event for `agent`, ordered chronologically.
    /// Powers the operator audit endpoint and the rebuild path
    /// ([`Self::rebuild_materialized`]).
    async fn list_events(&self, agent: AgentId) -> Result<Vec<MemoryEvent>, MemoryStoreError>;

    /// Rebuild `agent_memories` for a single agent by replaying the
    /// journal end to end. Used by the operator revert path (after
    /// appending an inverse event) and by tests asserting the materialized
    /// view is a deterministic projection of the events log.
    async fn rebuild_materialized(&self, agent: AgentId) -> Result<(), MemoryStoreError>;

    /// Top-K cosine-similarity search over `agent`'s memory embeddings.
    /// Rows without an embedding are skipped — the renderer and `recall`
    /// tool treat an empty result as a degraded layer rather than an error.
    async fn search_by_embedding(
        &self,
        agent: AgentId,
        embedding: &[f32],
        k: usize,
        filter: SearchFilter,
    ) -> Result<Vec<ScoredMemoryRow>, MemoryStoreError>;

    /// Pairs of an agent's memories whose cosine similarity ≥ `threshold`,
    /// excluding self-pairs. Each pair appears once (canonical ordering by
    /// id).
    async fn similar_pairs(
        &self,
        agent: AgentId,
        threshold: f32,
        max_pairs: usize,
    ) -> Result<Vec<PairCandidate>, MemoryStoreError>;

    /// Demote non-pinned `Validated` rows whose `last_validated_at` is
    /// older than `cutoff` to `Held` via the journal. Returns the count of
    /// rows demoted.
    async fn decay_validated(
        &self,
        agent: AgentId,
        cutoff: DateTime<Utc>,
    ) -> Result<usize, MemoryStoreError>;

    /// Passive time-survival promotion (doc/memory.md §1.8). Promote
    /// non-pinned `Tentative` rows whose `created_at` is older than
    /// `cutoff` and which are not referenced by any unresolved row in
    /// `contradiction_events` to `Held` via the journal. Returns the
    /// count of rows promoted.
    ///
    /// `last_validated_at` is NOT advanced — maturation is "absence of
    /// refutation," not independent evidence; the validation clock stays
    /// reserved for [`record_validation`].
    async fn mature_tentative(
        &self,
        agent: AgentId,
        cutoff: DateTime<Utc>,
    ) -> Result<usize, MemoryStoreError>;

    /// Stamp an independent-signal validation for the memory. Promotes the
    /// row's state per the lifecycle rules (Tentative → Held on first
    /// validation; Held → Validated on second). The journal is updated to
    /// reflect any state change so replay is faithful. Only the operator
    /// origin bypasses pin protection; agent and librarian origins against
    /// a pinned row return [`MemoryStoreError::PinnedImmutable`].
    async fn record_validation(
        &self,
        agent: AgentId,
        memory: MemoryId,
        origin: ValidationOrigin,
        detail: Option<&str>,
    ) -> Result<MemoryRow, MemoryStoreError>;

    /// Tenant-scoped variant of [`Self::record_validation`].
    async fn record_validation_for_user(
        &self,
        acting_user_id: UserId,
        agent: AgentId,
        memory: MemoryId,
        origin: ValidationOrigin,
        detail: Option<&str>,
    ) -> Result<MemoryRow, MemoryStoreError>;

    /// Insert a librarian-detected contradiction event for the given pair.
    /// Idempotent on `(memory_a, memory_b)` against currently-unresolved
    /// rows — duplicate insert returns the existing id.
    async fn record_contradiction(
        &self,
        agent: AgentId,
        a: MemoryId,
        b: MemoryId,
        reason: &str,
    ) -> Result<crate::memory::ContradictionEventId, MemoryStoreError>;

    /// List unresolved contradiction events for an agent, oldest first.
    async fn unresolved_contradictions(
        &self,
        agent: AgentId,
    ) -> Result<Vec<ContradictionEventRow>, MemoryStoreError>;

    /// Fetch one contradiction event by id.
    async fn read_contradiction(
        &self,
        id: crate::memory::ContradictionEventId,
    ) -> Result<Option<ContradictionEventRow>, MemoryStoreError>;

    /// Mark a contradiction event resolved. Idempotent: the underlying
    /// `WHERE resolved_at IS NULL` guard makes a second call against an
    /// already-closed row a no-op.
    async fn resolve_contradiction(
        &self,
        id: crate::memory::ContradictionEventId,
        outcome: ResolutionOutcome,
    ) -> Result<(), MemoryStoreError>;

    /// Tenant-scoped variant of [`Self::resolve_contradiction`].
    async fn resolve_contradiction_for_user(
        &self,
        acting_user_id: UserId,
        id: crate::memory::ContradictionEventId,
        outcome: ResolutionOutcome,
    ) -> Result<(), MemoryStoreError>;

    /// Force-evict the lowest-scoring non-pinned rows beyond `quota`.
    /// Returns ids of evicted memories.
    async fn evict_overflow(
        &self,
        agent: AgentId,
        quota: usize,
    ) -> Result<Vec<MemoryId>, MemoryStoreError>;

    /// Append an inverse-mutation event for the given journal id and rebuild
    /// the materialized row. Returns the row after revert (`None` when the
    /// revert removed the row).
    async fn revert_event(
        &self,
        agent: AgentId,
        event: MemoryEventId,
    ) -> Result<Option<MemoryRow>, MemoryStoreError>;

    /// Toggle the pinned flag on a row. Operator-only.
    async fn set_pinned(
        &self,
        agent: AgentId,
        memory: MemoryId,
        pinned: bool,
    ) -> Result<MemoryRow, MemoryStoreError>;

    /// Increment the access counter and bump `last_accessed_at` for the
    /// matching rows. Bounded — `ids.len()` ≤ `MAX_MEMORIES_PER_AGENT`.
    /// Reading does NOT advance validation (doc/memory.md §1.7).
    async fn record_access(&self, ids: &[MemoryId]) -> Result<(), MemoryStoreError>;

    /// Tenant-scoped variant of [`Self::record_access`].
    async fn record_access_for_user(
        &self,
        acting_user_id: UserId,
        ids: &[MemoryId],
    ) -> Result<(), MemoryStoreError>;
}

/// One row in `contradiction_events`. Three valid shapes mirror the
/// `contradiction_events_resolved_consistent` CHECK:
/// * pending — every resolution column is `None`.
/// * mutation close — `resolved_at` + `resolution_event_id` set, reason `None`.
/// * no-action close — `resolved_at` + `resolution_reason` set, event_id `None`.
#[derive(Debug, Clone)]
pub struct ContradictionEventRow {
    pub id: crate::memory::ContradictionEventId,
    pub agent_id: AgentId,
    /// Owning org of the parent agent, denormalised by migration 17.
    pub org_id: OrgId,
    pub memory_a: MemoryId,
    pub memory_b: MemoryId,
    pub reason: String,
    pub created_at: DateTime<Utc>,
    pub resolved_at: Option<DateTime<Utc>>,
    pub resolution_event_id: Option<MemoryEventId>,
    pub resolution_reason: Option<String>,
}

/// Free-text rationale persisted on a no-action contradiction close. Smart
/// constructor at the boundary so the column's 1..=`CONTRADICTION_REASON_MAX_BYTES`
/// length invariant is encoded in the type.
#[derive(Debug, Clone)]
pub struct ResolutionReason(String);

impl ResolutionReason {
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    #[must_use]
    pub fn into_inner(self) -> String {
        self.0
    }
}

impl TryFrom<String> for ResolutionReason {
    type Error = ParseError;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        let len = value.len();
        if len == 0 {
            return Err(ParseError::Empty {
                field: "resolution_reason",
            });
        }
        if len > CONTRADICTION_REASON_MAX_BYTES {
            return Err(ParseError::TooLong {
                field: "resolution_reason",
                max: CONTRADICTION_REASON_MAX_BYTES,
                got: len,
            });
        }
        Ok(Self(value))
    }
}

/// Outcome of a resolution turn — fed to [`MemoryStore::resolve_contradiction`].
///
/// The two variants map to the two non-pending shapes of the
/// `contradiction_events_resolved_consistent` CHECK, so the invalid
/// combination (both / neither) is unrepresentable.
#[derive(Debug, Clone)]
pub enum ResolutionOutcome {
    /// A mutation tool closed the pair inline; `event_id` points at the
    /// journal row that did it.
    Mutation(MemoryEventId),
    /// The resolution turn ended without mutating either memory; `reason`
    /// is the assistant's final text (truncated to the column cap).
    NoAction { reason: ResolutionReason },
}

/// Free-text evidence persisted on the `validation_events.detail` column.
/// Smart constructor at the boundary so the column's
/// 1..=`VALIDATION_EVIDENCE_MAX_BYTES` length invariant is encoded in the
/// type.
#[derive(Debug, Clone)]
pub struct MemoryEvidence(String);

impl MemoryEvidence {
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    #[must_use]
    pub fn into_inner(self) -> String {
        self.0
    }
}

impl TryFrom<String> for MemoryEvidence {
    type Error = ParseError;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        let len = value.len();
        if len == 0 {
            return Err(ParseError::Empty {
                field: "memory_evidence",
            });
        }
        if len > VALIDATION_EVIDENCE_MAX_BYTES {
            return Err(ParseError::TooLong {
                field: "memory_evidence",
                max: VALIDATION_EVIDENCE_MAX_BYTES,
                got: len,
            });
        }
        Ok(Self(value))
    }
}

/// Wire label persisted on `validation_events.source`.
///
/// Internal to the store; callers reach `record_validation` through
/// [`ValidationOrigin`], which encodes the audit/journal pairing as a
/// single invariant. Per doc/memory.md §1.7, validation only advances on
/// genuinely independent signals — the librarian's mechanical dedup is no
/// longer a validation source (a same-session re-emergence is self-
/// citation; a cross-session re-emergence is typically just the prior
/// memory being loaded into the new session's stable layer and re-stated).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValidationSource {
    /// External confirmation: an agent's own follow-up turn confirmed it
    /// (web_search / web_fetch / peer-agent reply / human via
    /// send_message / user affirmation in the current turn).
    ExternalConfirmation,
    /// Operator endorsement: a `manager_note` flagged the memory as
    /// validated.
    OperatorEndorsement,
}

impl ValidationSource {
    /// Wire label used in the column constraint.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ExternalConfirmation => "external_confirmation",
            Self::OperatorEndorsement => "operator_endorsement",
        }
    }
}

/// Origin of a [`MemoryStore::record_validation`] call.
///
/// Pairs the audit signal ([`ValidationSource`]) with the journal Update
/// event's provenance ([`MutationSource`]) so a caller cannot accidentally
/// emit an invalid combination — e.g. `OperatorEndorsement` attributed to
/// a `Turn(_)`. The store derives both labels from this single enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValidationOrigin {
    /// Agent-driven external confirmation during a turn (web_search,
    /// web_fetch, peer reply, human-via-send_message, user affirmation).
    Agent(PromptRequestId),
    /// Operator endorsement via `manager_note`. Bypasses pin protection.
    Operator,
}

impl ValidationOrigin {
    #[must_use]
    pub const fn source(self) -> ValidationSource {
        match self {
            Self::Agent(_) => ValidationSource::ExternalConfirmation,
            Self::Operator => ValidationSource::OperatorEndorsement,
        }
    }

    #[must_use]
    pub const fn mutation_source(self) -> MutationSource {
        match self {
            Self::Agent(id) => MutationSource::Turn(id),
            Self::Operator => MutationSource::Operator,
        }
    }
}

/// Cheap-clone handle so collaborators can hold the store without a generic.
pub type SharedMemoryStore = Arc<dyn MemoryStore>;
