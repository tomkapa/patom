//! Domain primitives for the memory subsystem (doc/memory.md §1.2).
//!
//! CLAUDE.md §1: every value carrying an invariant is a newtype with a
//! `TryFrom` smart constructor. The storage layer and the tool layer bind
//! these types directly through the sqlx `Type` / `Encode` / `Decode`
//! impls; raw `String`/`Uuid` does not cross the module boundary.

use std::fmt;
use std::sync::Arc;

use crate::types::ParseError;

use super::limits::{MEMORY_CONTENT_MAX_BYTES, RECALL_DEFAULT_RESULTS, RECALL_MAX_RESULTS};

crate::uuid_newtype! {
    /// Opaque identifier for a row in `agent_memories`. The operator UI
    /// surfaces a short [`MemoryHandle`] derived from a per-session map.
    pub MemoryId
}

crate::uuid_newtype! {
    /// Opaque identifier for a row in `memory_events` — every mutation
    /// produces one. The operator revert path references these ids to undo
    /// a past change by appending the inverse event.
    pub MemoryEventId
}

crate::uuid_newtype! {
    /// Opaque identifier for a row in `contradiction_events`. Carried on
    /// `prompt_requests.kind_payload` for resolution-turn dispatch.
    pub ContradictionEventId
}

crate::uuid_newtype! {
    /// Opaque identifier for a row in `validation_events`. The validation
    /// log is append-only; ids surface only in the operator audit view.
    pub ValidationEventId
}

crate::str_enum! {
    /// What the memory is about (doc/memory.md §1.2). The label list below
    /// is the single source of truth — the column `CHECK` constraint, the
    /// JSON wire format, and the system-prompt rendering all key off these
    /// strings.
    ///
    /// Variant names diverge from the doc's "Self" because `Self` is a
    /// reserved Rust keyword; the wire label stays `"self"` so the column
    /// constraint and operator UI are unaffected.
    pub enum MemoryKind {
        /// Identity, style, preferences ("I default to terse replies").
        Identity     => "self",
        /// Beliefs about specific peers or humans (external).
        Other        => "other",
        /// Beliefs about other *agents* in the network, written from the
        /// agent's delegation experience (doc/agent_discovery_plan.md §4).
        Collaborator => "collaborator",
        /// Learned how-tos.
        Procedure    => "procedure",
        /// Known unknowns.
        Open         => "open",
    }
}

impl MemoryKind {
    /// Capitalised label used as the section header in the rendered
    /// system prompt (`### Self`, `### Other`, …). Distinct from the
    /// lowercase wire label returned by `as_str` so the storage format
    /// can stay snake-case while the model-facing prompt reads as prose.
    #[must_use]
    pub const fn display_label(self) -> &'static str {
        match self {
            Self::Identity => "Self",
            Self::Other => "Other",
            Self::Collaborator => "Collaborator",
            Self::Procedure => "Procedure",
            Self::Open => "Open",
        }
    }
}

crate::str_enum! {
    /// Confidence/lifecycle bucket (doc/memory.md §1.2). The agent only
    /// reasons about these qualitative states; the underlying numeric
    /// confidence stays hidden — calibration drift is what the qualitative
    /// scale is designed to avoid.
    pub enum MemoryState {
        /// Operator-pinned, immutable to the agent.
        Core      => "core",
        /// Confirmed by independent signals.
        Validated => "validated",
        /// Default accepted.
        Held      => "held",
        /// Newly written, unverified.
        Tentative => "tentative",
    }
}

impl MemoryState {
    /// Higher = more trusted; drives the stable-layer prompt order and the
    /// eviction priority used by `evict_overflow`.
    #[must_use]
    pub const fn priority(self) -> u8 {
        match self {
            Self::Core => 4,
            Self::Validated => 3,
            Self::Held => 2,
            Self::Tentative => 1,
        }
    }
}

crate::str_enum! {
    /// Origin of a memory mutation. Distinguishes agent-driven writes
    /// (carried back to the producing turn) from operator-authored notes
    /// and librarian-driven cleanup. Used both on the journal row and to
    /// authorise pinned-memory edits (operator path bypasses the agent
    /// pinned-immunity check; agent path does not).
    pub enum MutationSourceKind {
        Turn      => "turn",
        Operator  => "operator",
        Librarian => "librarian",
    }
}

crate::str_enum! {
    /// Mutation type recorded on every `memory_events` row. The text
    /// labels match the column `CHECK` constraint exactly.
    pub enum MutationKind {
        Write  => "write",
        Update => "update",
        Forget => "forget",
    }
}

/// Validated memory text. Non-empty; capped at
/// [`MEMORY_CONTENT_MAX_BYTES`]. Reference-counted so the in-process layers
/// (renderer, retrieval cache) can hand the same allocation around without
/// copying.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct MemoryContent(Arc<str>);

impl MemoryContent {
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl TryFrom<&str> for MemoryContent {
    type Error = ParseError;

    fn try_from(raw: &str) -> Result<Self, Self::Error> {
        if raw.trim().is_empty() {
            return Err(ParseError::Empty {
                field: "memory_content",
            });
        }
        if raw.len() > MEMORY_CONTENT_MAX_BYTES {
            return Err(ParseError::TooLong {
                field: "memory_content",
                max: MEMORY_CONTENT_MAX_BYTES,
                got: raw.len(),
            });
        }
        Ok(Self(Arc::from(raw)))
    }
}

impl TryFrom<String> for MemoryContent {
    type Error = ParseError;
    fn try_from(raw: String) -> Result<Self, Self::Error> {
        Self::try_from(raw.as_str())
    }
}

impl fmt::Debug for MemoryContent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("MemoryContent").field(&self.0.len()).finish()
    }
}

impl fmt::Display for MemoryContent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Short, per-session, stable handle the agent uses in tool calls.
///
/// Renders as `M-NN`. Round-trips through [`fmt::Display`] /
/// [`TryFrom<&str>`] so a tool-call argument that came back through the
/// model can be parsed without ambiguity. The (handle ↔ UUID) map is
/// built per session at prompt assembly time and lives next to the
/// renderer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct MemoryHandle(u32);

impl MemoryHandle {
    /// Hard ceiling on per-session handles. Token-driven budgets cap the
    /// rendered section bytes; this is the numeric ceiling so handle
    /// parsing cannot accept arbitrarily large integers. Comfortably
    /// above any plausible per-session memory count.
    pub const MAX: Self = Self(9_999);

    #[must_use]
    pub const fn get(self) -> u32 {
        self.0
    }
}

impl TryFrom<u32> for MemoryHandle {
    type Error = ParseError;

    fn try_from(n: u32) -> Result<Self, Self::Error> {
        if n == 0 || n > Self::MAX.0 {
            return Err(ParseError::OutOfRange {
                field: "memory_handle",
                detail: "1..=9999",
            });
        }
        Ok(Self(n))
    }
}

impl TryFrom<&str> for MemoryHandle {
    type Error = ParseError;

    fn try_from(raw: &str) -> Result<Self, Self::Error> {
        // Accept the canonical `M-NN` form only — the agent always sees
        // this exact shape so a free-form integer would be a bug somewhere
        // upstream.
        let digits = raw.strip_prefix("M-").ok_or(ParseError::Malformed {
            field: "memory_handle",
            detail: "expected `M-<n>` form",
        })?;
        let n: u32 = digits.parse().map_err(|_| ParseError::Malformed {
            field: "memory_handle",
            detail: "non-numeric handle suffix",
        })?;
        Self::try_from(n)
    }
}

impl fmt::Display for MemoryHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "M-{}", self.0)
    }
}

/// Bounded top-K for the `recall` tool. Parsed at the JSON boundary —
/// holding a `RecallLimit` proves the value is in `1..=RECALL_MAX_RESULTS`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecallLimit(u8);

impl RecallLimit {
    /// Default top-K used when the tool input omits `limit`. Encoded as
    /// the const so `serde(default)` can supply it without an `unwrap_or`
    /// at the call site.
    pub const DEFAULT: Self = Self(RECALL_DEFAULT_RESULTS);

    #[must_use]
    pub const fn get(self) -> u8 {
        self.0
    }
}

impl Default for RecallLimit {
    fn default() -> Self {
        Self::DEFAULT
    }
}

impl TryFrom<u8> for RecallLimit {
    type Error = ParseError;
    fn try_from(n: u8) -> Result<Self, Self::Error> {
        if n == 0 || n > RECALL_MAX_RESULTS {
            return Err(ParseError::OutOfRange {
                field: "recall_limit",
                detail: "1..=RECALL_MAX_RESULTS",
            });
        }
        Ok(Self(n))
    }
}

impl<'de> serde::Deserialize<'de> for RecallLimit {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let n = u8::deserialize(d)?;
        Self::try_from(n).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_content_rejects_empty_and_oversize() {
        assert!(MemoryContent::try_from("").is_err());
        assert!(MemoryContent::try_from("   \n\t").is_err());
        let big = "a".repeat(MEMORY_CONTENT_MAX_BYTES + 1);
        assert!(MemoryContent::try_from(big.as_str()).is_err());
    }

    #[test]
    fn memory_content_accepts_normal() {
        let c = MemoryContent::try_from("I default to terse replies.").expect("valid");
        assert_eq!(c.as_str(), "I default to terse replies.");
    }

    #[test]
    fn memory_handle_round_trips() {
        let h = MemoryHandle::try_from(12u32).expect("valid");
        let s = h.to_string();
        assert_eq!(s, "M-12");
        let parsed = MemoryHandle::try_from(s.as_str()).expect("parse");
        assert_eq!(parsed, h);
    }

    #[test]
    fn memory_handle_rejects_zero_and_overflow() {
        assert!(MemoryHandle::try_from(0u32).is_err());
        assert!(MemoryHandle::try_from(MemoryHandle::MAX.get() + 1).is_err());
    }

    #[test]
    fn memory_handle_rejects_malformed_strings() {
        assert!(MemoryHandle::try_from("12").is_err());
        assert!(MemoryHandle::try_from("M-").is_err());
        assert!(MemoryHandle::try_from("M-abc").is_err());
        assert!(MemoryHandle::try_from("X-12").is_err());
    }

    #[test]
    fn memory_kind_round_trips_every_variant() {
        for k in MemoryKind::ALL.iter().copied() {
            assert_eq!(MemoryKind::parse(k.as_str()), Some(k));
        }
        assert_eq!(MemoryKind::parse("nope"), None);
        // Wire label sanity — the renderer and the column CHECK both
        // depend on `"self"` being the Identity label.
        assert_eq!(MemoryKind::Identity.as_str(), "self");
    }

    #[test]
    fn memory_state_round_trips_every_variant() {
        for s in MemoryState::ALL.iter().copied() {
            assert_eq!(MemoryState::parse(s.as_str()), Some(s));
        }
    }
}
