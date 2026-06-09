//! Domain primitives for the prompt pipeline. CLAUDE.md §1 — every value carrying an
//! invariant is wrapped in a newtype with a `TryFrom` smart constructor.
//!
//! `sqlx::Type` / `Encode` / `Decode` impls let storage code bind these values
//! directly (`.bind(RequestStatus::Pending)`, `fetch_one::<(_, TurnSeq, _)>()`),
//! removing every hand-rolled string match against the `prompt_requests.status`
//! check constraint. Live next to the type so a new variant cannot drift past
//! the wire mapping.

use std::fmt;

use serde::{Deserialize, Serialize};
use sqlx::Postgres;
use sqlx::encode::IsNull;
use sqlx::error::BoxDynError;
use sqlx::postgres::{PgArgumentBuffer, PgTypeInfo, PgValueRef};
use sqlx::{Decode, Encode, Type};
use thiserror::Error;
use uuid::Uuid;

use crate::memory::ContradictionEventId;
use crate::threads::{ThreadId, ThreadMessageId};
use crate::types::ParseError;

use super::limits::{MAX_ATTEMPTS, MAX_IDEMPOTENCY_KEY_BYTES};

/// Decode a Postgres `BIGINT` into a `u64`, asserting non-negativity. The
/// sequence columns are `BIGINT` only because Postgres has no unsigned 64-bit
/// type; their values are always non-negative by construction (counters only
/// grow). A negative value means schema corruption per CLAUDE.md §6.
fn decode_u64(value: PgValueRef<'_>, name: &'static str) -> Result<u64, BoxDynError> {
    let raw = <i64 as Decode<Postgres>>::decode(value)?;
    u64::try_from(raw)
        .map_err(|_| format!("invariant: {name} must be non-negative, got {raw}").into())
}

/// Encode a `u64` as a Postgres `BIGINT`, panicking if the value crosses
/// `i64::MAX`. Sequence values reach that bound only after ~9.2×10¹⁸
/// increments — astronomical for any realistic workload — so observing it is a
/// program error, not a recoverable failure (CLAUDE.md §6).
fn encode_u64(
    n: u64,
    buf: &mut PgArgumentBuffer,
    name: &'static str,
) -> Result<IsNull, BoxDynError> {
    let raw = i64::try_from(n).unwrap_or_else(|_| panic!("invariant: {name} fits in i64"));
    <i64 as Encode<Postgres>>::encode_by_ref(&raw, buf)
}

/// Sequence number stepped past `u64::MAX`. Astronomical, but typed so callers know
/// what failed without parsing a string.
#[derive(Debug, Clone, Copy, Error)]
#[error("{seq} overflow")]
pub struct SeqOverflow {
    /// Name of the sequence that overflowed (`"turn_seq"`, `"chunk_seq"`).
    pub seq: &'static str,
}

crate::uuid_newtype! {
    /// Opaque request id — minted by the queue when a prompt is enqueued.
    pub PromptRequestId
}

impl TryFrom<&str> for PromptRequestId {
    type Error = ParseError;
    fn try_from(raw: &str) -> Result<Self, Self::Error> {
        Uuid::parse_str(raw)
            .map(Self::from)
            .map_err(|_| ParseError::Malformed {
                field: "request_id",
                detail: "not a UUID",
            })
    }
}

crate::uuid_newtype! {
    /// Identifier for an individual worker in the pool. Carried on lease tokens so we can
    /// trace which worker held a session.
    pub WorkerId
}

/// Monotonically-increasing sequence number bumped on every claim. Stale writes from
/// a zombie worker are rejected when the token's seq does not match the current row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TurnSeq(u64);

impl TurnSeq {
    pub const ZERO: Self = Self(0);

    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }

    /// Step to the next sequence. Returns [`SeqOverflow`] on overflow — astronomical,
    /// but typed per CLAUDE.md §12.
    pub const fn next(self) -> Result<Self, SeqOverflow> {
        match self.0.checked_add(1) {
            Some(n) => Ok(Self(n)),
            None => Err(SeqOverflow { seq: "turn_seq" }),
        }
    }
}

impl fmt::Display for TurnSeq {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl Type<Postgres> for TurnSeq {
    fn type_info() -> PgTypeInfo {
        <i64 as Type<Postgres>>::type_info()
    }
    fn compatible(ty: &PgTypeInfo) -> bool {
        <i64 as Type<Postgres>>::compatible(ty)
    }
}

impl<'r> Decode<'r, Postgres> for TurnSeq {
    fn decode(value: PgValueRef<'r>) -> Result<Self, BoxDynError> {
        decode_u64(value, "turn_seq").map(Self)
    }
}

impl Encode<'_, Postgres> for TurnSeq {
    fn encode_by_ref(&self, buf: &mut PgArgumentBuffer) -> Result<IsNull, BoxDynError> {
        encode_u64(self.0, buf, "turn_seq")
    }
}

/// Number of attempts a request has been claimed. Capped at [`MAX_ATTEMPTS`]; one over
/// the cap means the row is parked with reason = poison.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Attempts(u32);

impl Attempts {
    pub const ZERO: Self = Self(0);

    #[must_use]
    pub const fn get(self) -> u32 {
        self.0
    }

    /// Bump and return whether the new value is still below the poison threshold.
    pub fn increment(&mut self) -> AttemptOutcome {
        self.0 = self.0.saturating_add(1);
        if self.0 >= MAX_ATTEMPTS {
            AttemptOutcome::Poisoned
        } else {
            AttemptOutcome::Live
        }
    }
}

/// Outcome of an [`Attempts::increment`] call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttemptOutcome {
    Live,
    Poisoned,
}

/// A free-form, client-supplied idempotency key. Bounded length so the dedup index
/// (today: a `HashMap`; tomorrow: a Postgres unique index) cannot grow unboundedly.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct IdempotencyKey(String);

impl IdempotencyKey {
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for IdempotencyKey {
    type Error = ParseError;
    fn try_from(raw: String) -> Result<Self, Self::Error> {
        if raw.is_empty() {
            return Err(ParseError::Empty {
                field: "idempotency_key",
            });
        }
        if raw.len() > MAX_IDEMPOTENCY_KEY_BYTES {
            return Err(ParseError::TooLong {
                field: "idempotency_key",
                max: MAX_IDEMPOTENCY_KEY_BYTES,
                got: raw.len(),
            });
        }
        Ok(Self(raw))
    }
}

impl TryFrom<&str> for IdempotencyKey {
    type Error = ParseError;
    fn try_from(raw: &str) -> Result<Self, Self::Error> {
        Self::try_from(raw.to_string())
    }
}

crate::str_enum! {
    /// Lifecycle state of a prompt request row. The pipeline is forward-only:
    /// `Pending -> Processing -> Done | Failed`.
    ///
    /// The label list below is the single source of truth — the
    /// `prompt_requests.status` `CHECK` constraint and the JSON wire format
    /// are keyed off these strings exactly. Adding a variant means one edit
    /// here and one matching migration; `as_str`, `parse`, and the
    /// sqlx/serde glue follow automatically.
    pub enum RequestStatus {
        Pending    => "pending",
        Processing => "processing",
        Done       => "done",
        Failed     => "failed",
    }
}

impl RequestStatus {
    /// Terminal states — no further transitions possible. Used by the pre-turn
    /// check and the cancel watcher; expressed once here so a new terminal variant
    /// cannot be missed at one of those call sites.
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Done | Self::Failed)
    }
}

crate::str_enum! {
    /// Job kind carried on every `prompt_requests` row (doc/memory.md §2.1).
    /// Generalises the queue from "queue of prompts" to "queue of agent
    /// jobs"; the worker dispatches on this column to pick reply / reflect
    /// / resolve. The label list is the single source of truth for the
    /// column `CHECK` constraint and the JSON wire format used by
    /// [`RequestKindPayload`].
    pub enum RequestKind {
        /// User-facing reply turn. Carries the empty
        /// [`RequestKindPayload::Normal`] variant.
        Normal     => "normal",
        /// Autonomous self-curation. Payload is
        /// [`RequestKindPayload::Reflection`].
        Reflection => "reflection",
        /// Single-contradiction resolution. Payload is
        /// [`RequestKindPayload::Resolution`].
        Resolution => "resolution",
    }
}

/// Kind-specific metadata persisted in `prompt_requests.kind_payload` as JSONB.
///
/// Every [`RequestKind`] has a corresponding variant — `Normal` is an
/// empty struct today, but matching shapes 1:1 with `RequestKind` keeps
/// the API symmetric and makes "is this a Normal claim?" a single match
/// arm rather than `Option::is_none` plus a kind check. The empty struct
/// (vs. unit) variant leaves room for Normal-specific fields without a
/// wire-format break.
///
/// Adjacently-tagged JSON (`{"kind":"normal","data":{}}`) so a new variant
/// cannot collide with an existing one — and so `serde_json::from_value`
/// rejects an old-shaped payload deterministically rather than silently
/// matching the wrong arm.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum RequestKindPayload {
    Normal {},
    Reflection {
        /// Thread this reflection covers — its `reflection_checkpoints`
        /// `(agent_id, thread_id)` row is the audit anchor.
        thread_id: ThreadId,
        /// Frozen-slice upper bound the scheduler captured at enqueue time. The
        /// conversation slice spans `(previous checkpoint, up_to_message_id]`;
        /// on success the checkpoint is stamped to this `thread_messages.id` so
        /// the next reflection picks up strictly after it. NOT read-at-run — the
        /// slice is frozen at enqueue so late chat doesn't shift what was reflected.
        up_to_message_id: ThreadMessageId,
    },
    Resolution {
        contradiction_event_id: ContradictionEventId,
    },
}

impl RequestKindPayload {
    /// Discriminator — the [`RequestKind`] this payload corresponds to.
    /// Used at insert time to assert (kind, payload) consistency before
    /// the row hits Postgres.
    #[must_use]
    pub const fn kind(&self) -> RequestKind {
        match self {
            Self::Normal {} => RequestKind::Normal,
            Self::Reflection { .. } => RequestKind::Reflection,
            Self::Resolution { .. } => RequestKind::Resolution,
        }
    }
}

/// Why a request was marked failed. Closed enum so every caller of `mark_failed` is
/// exhaustive at compile time.
///
/// Persisted to `prompt_requests.failure_reason TEXT` as adjacently-tagged JSON
/// (`{"type":"provider","detail":"..."}`) via [`Encode`]/[`Decode`]. The same
/// serde representation is the wire format for any future API consumer; the
/// human-friendly [`fmt::Display`] is for logs/SSE chunks only and is **not**
/// the storage format — so changing `Display` cannot silently corrupt rows.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "detail", rename_all = "snake_case")]
pub enum FailureReason {
    Cancelled,
    Timeout,
    Provider(String),
    Hook(String),
    /// Hit `MAX_ATTEMPTS` without ever succeeding.
    Poison,
    /// A precondition the worker can never recover from (e.g. session vanished).
    Unrecoverable(String),
    /// The DAG's `turns_used` reached `turns_cap`; the offending
    /// `send_message` insert was rolled back. Surfaced as a terminal chunk
    /// on the root request's stream so the caller learns the conversation
    /// hit its loop budget.
    DagBudgetExceeded,
    /// The org reached its monthly spend cap mid-conversation. The per-turn
    /// budget gate ([`crate::agent_core::AgentError::BudgetExceeded`]) stopped
    /// the turn before the provider call; surfaced as a terminal chunk so the
    /// caller learns the workspace ran out of budget.
    BudgetExceeded,
    /// Agent produced text without ever calling `send_message`. The worker
    /// nudged it `MAX_PINGPONG_RETRIES` times and still got no delivery —
    /// the request is parked so the caller knows the model misbehaved.
    NoEgress,
}

impl FailureReason {
    /// Stable, low-cardinality label for tracing attributes.
    #[must_use]
    pub const fn label(&self) -> &'static str {
        match self {
            Self::Cancelled => "cancelled",
            Self::Timeout => "timeout",
            Self::Provider(_) => "provider",
            Self::Hook(_) => "hook",
            Self::Poison => "poison",
            Self::Unrecoverable(_) => "unrecoverable",
            Self::DagBudgetExceeded => "dag_budget_exceeded",
            Self::BudgetExceeded => "budget_exceeded",
            Self::NoEgress => "no_egress",
        }
    }
}

impl fmt::Display for FailureReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cancelled => f.write_str("cancelled"),
            Self::Timeout => f.write_str("turn timed out"),
            Self::Provider(s) => write!(f, "provider error: {s}"),
            Self::Hook(s) => write!(f, "hook denied: {s}"),
            Self::Poison => f.write_str("max attempts exceeded"),
            Self::Unrecoverable(s) => write!(f, "unrecoverable: {s}"),
            Self::DagBudgetExceeded => f.write_str("dag turn budget exceeded"),
            Self::BudgetExceeded => f.write_str("monthly spend budget exceeded"),
            Self::NoEgress => {
                f.write_str("agent produced text without calling send_message after retries")
            }
        }
    }
}

impl Type<Postgres> for FailureReason {
    fn type_info() -> PgTypeInfo {
        <&str as Type<Postgres>>::type_info()
    }
    fn compatible(ty: &PgTypeInfo) -> bool {
        <&str as Type<Postgres>>::compatible(ty)
    }
}

impl<'r> Decode<'r, Postgres> for FailureReason {
    fn decode(value: PgValueRef<'r>) -> Result<Self, BoxDynError> {
        let raw = <&str as Decode<'r, Postgres>>::decode(value)?;
        // §6: rows only land here via the `Encode` impl below, so a parse failure
        // means the schema has been hand-edited or a previous deploy used a stale
        // serializer. Surface it as a backend error rather than coercing.
        serde_json::from_str(raw).map_err(|e| {
            format!("invariant: failure_reason JSON decode failed for {raw:?}: {e}").into()
        })
    }
}

impl<'q> Encode<'q, Postgres> for FailureReason {
    fn encode_by_ref(&self, buf: &mut PgArgumentBuffer) -> Result<IsNull, BoxDynError> {
        let json = serde_json::to_string(self)
            .expect("invariant: FailureReason serialises infallibly via serde_json");
        <String as Encode<'q, Postgres>>::encode(json, buf)
    }
}

/// Sequence number for an individual chunk on a request stream. Strictly monotonic
/// per request so an SSE client reconnecting can pass `Last-Event-ID` to skip already-
/// observed chunks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ChunkSeq(u64);

impl ChunkSeq {
    pub const ZERO: Self = Self(0);

    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }

    /// Step to the next sequence. Returns [`SeqOverflow`] on overflow.
    pub const fn next(self) -> Result<Self, SeqOverflow> {
        match self.0.checked_add(1) {
            Some(n) => Ok(Self(n)),
            None => Err(SeqOverflow { seq: "chunk_seq" }),
        }
    }
}

impl From<u64> for ChunkSeq {
    fn from(n: u64) -> Self {
        Self(n)
    }
}

impl fmt::Display for ChunkSeq {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl Type<Postgres> for ChunkSeq {
    fn type_info() -> PgTypeInfo {
        <i64 as Type<Postgres>>::type_info()
    }
    fn compatible(ty: &PgTypeInfo) -> bool {
        <i64 as Type<Postgres>>::compatible(ty)
    }
}

impl<'r> Decode<'r, Postgres> for ChunkSeq {
    fn decode(value: PgValueRef<'r>) -> Result<Self, BoxDynError> {
        decode_u64(value, "chunk_seq").map(Self)
    }
}

impl Encode<'_, Postgres> for ChunkSeq {
    fn encode_by_ref(&self, buf: &mut PgArgumentBuffer) -> Result<IsNull, BoxDynError> {
        encode_u64(self.0, buf, "chunk_seq")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idempotency_key_rejects_empty_and_oversize() {
        assert!(IdempotencyKey::try_from("").is_err());
        assert!(IdempotencyKey::try_from("a".repeat(MAX_IDEMPOTENCY_KEY_BYTES + 1)).is_err());
        let k = IdempotencyKey::try_from("ok").expect("valid");
        assert_eq!(k.as_str(), "ok");
    }

    #[test]
    fn turn_seq_advances_monotonically() {
        let s = TurnSeq::ZERO;
        let s1 = s.next().expect("under cap");
        assert!(s1 > s);
    }

    #[test]
    fn attempts_poisons_at_cap() {
        let mut a = Attempts::ZERO;
        for _ in 0..(MAX_ATTEMPTS - 1) {
            assert_eq!(a.increment(), AttemptOutcome::Live);
        }
        assert_eq!(a.increment(), AttemptOutcome::Poisoned);
    }

    #[test]
    fn request_status_string_round_trips() {
        // `as_str`/`parse` together are the single source of truth shared with the
        // CHECK constraint on `prompt_requests.status` and exercised by `Decode` at
        // the Pg seam. Iterating `ALL` guarantees a new variant is round-tripped
        // here without an extra test edit.
        for s in RequestStatus::ALL.iter().copied() {
            assert_eq!(RequestStatus::parse(s.as_str()), Some(s));
        }
        assert_eq!(RequestStatus::parse("nope"), None);
    }

    #[test]
    fn request_status_terminal_only_for_done_and_failed() {
        assert!(RequestStatus::Done.is_terminal());
        assert!(RequestStatus::Failed.is_terminal());
        assert!(!RequestStatus::Pending.is_terminal());
        assert!(!RequestStatus::Processing.is_terminal());
    }
}
