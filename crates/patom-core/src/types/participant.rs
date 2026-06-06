//! Conversation participant — Human or a registered Agent.
//!
//! Multi-agent communication makes sessions strictly 2-party. Each session has
//! two participants drawn from this enum. CLAUDE.md §1 says to encode invariants
//! in types — `Participant` replaces three implicit conventions (a nullable
//! `agent_id`, a `role` string, and ad-hoc `is_human` checks) with one closed
//! sum.
//!
//! Storage shape — for tables that persist participants the column pair is
//! `(kind TEXT, agent_id UUID NULL)` with a `CHECK ((kind='agent') = (agent_id
//! IS NOT NULL))`. Every encoder/decoder in this crate funnels through
//! [`ParticipantKind::as_str`] so the wire/storage label cannot drift from the
//! enum. The serde wire form is `{"kind":"human"}` or
//! `{"kind":"agent","agent_id":"..."}`.

use std::cmp::Ordering;
use std::fmt;

use serde::{Deserialize, Serialize};

use crate::agents::AgentId;

/// One end of a session.
///
/// Constructed only via [`Self::human`] / [`Self::agent`] / [`Self::system`] —
/// there is no public field, so the only valid shapes are the three below.
/// `System` is the synthetic singleton counterpart used by autonomous
/// agent-only sessions (reflection, resolution per doc/memory.md §1.6, §1.8):
/// the agent talks to itself for audit; the System side never speaks back.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Participant {
    Human,
    Agent {
        agent_id: AgentId,
    },
    /// Synthetic counterpart for reflection / resolution sessions. The agent
    /// is paired with `System` so the canonical-pair invariant holds without
    /// relaxing it for self-sessions; nobody on this side speaks.
    System,
}

impl Participant {
    /// The human end of a session.
    #[must_use]
    pub const fn human() -> Self {
        Self::Human
    }

    /// An agent end of a session.
    #[must_use]
    pub const fn agent(id: AgentId) -> Self {
        Self::Agent { agent_id: id }
    }

    /// The synthetic system end of a session — used for off-conversation
    /// agent work (reflection, resolution).
    #[must_use]
    pub const fn system() -> Self {
        Self::System
    }

    /// Tag without payload — the value persisted in the `*_kind TEXT` column.
    #[must_use]
    pub const fn kind(self) -> ParticipantKind {
        match self {
            Self::Human => ParticipantKind::Human,
            Self::Agent { .. } => ParticipantKind::Agent,
            Self::System => ParticipantKind::System,
        }
    }

    /// `Some(id)` for the agent variant, `None` for human or system.
    #[must_use]
    pub const fn agent_id(self) -> Option<AgentId> {
        match self {
            Self::Human | Self::System => None,
            Self::Agent { agent_id } => Some(agent_id),
        }
    }

    /// True iff this is the human end.
    #[must_use]
    pub const fn is_human(self) -> bool {
        matches!(self, Self::Human)
    }

    /// True iff this is an agent end.
    #[must_use]
    pub const fn is_agent(self) -> bool {
        matches!(self, Self::Agent { .. })
    }

    /// True iff this is the synthetic system end.
    #[must_use]
    pub const fn is_system(self) -> bool {
        matches!(self, Self::System)
    }

    /// Canonical ordering for session deduplication. Returns `(a, b)` such
    /// that `a < b` per [`Self::canonical_cmp`]. Used by the `sessions`
    /// upsert to compute a stable `(participant_a, participant_b)` slot
    /// regardless of caller direction.
    ///
    /// Returns `None` if `lhs == rhs` — a self-session is representationally
    /// invalid; callers must reject before calling here.
    #[must_use]
    pub fn canonical_pair(lhs: Self, rhs: Self) -> Option<(Self, Self)> {
        match Self::canonical_cmp(&lhs, &rhs) {
            Ordering::Less => Some((lhs, rhs)),
            Ordering::Greater => Some((rhs, lhs)),
            Ordering::Equal => None,
        }
    }

    /// Total ordering used to canonicalise pairs.
    ///
    /// Matches the Postgres CHECK constraint `sessions_participants_distinct`,
    /// which uses tuple `<` and therefore the SQL string ordering of the
    /// `participant_*_kind` column. Lex order on the kind labels is
    /// `'agent' < 'human' < 'system'`, so Rust mirrors: `Agent < Human <
    /// System`. Two `Agent` values then sort by their `AgentId`'s `Uuid`
    /// order. Both sides agreeing on the canonical order is what makes the
    /// `sessions_dag_pair_unique` upsert idempotent across callers.
    pub fn canonical_cmp(lhs: &Self, rhs: &Self) -> Ordering {
        // Order each variant by lex of its kind label first; equal kinds
        // tiebreak by payload (only Agent has one). Mirrors the Postgres
        // tuple `<` on `(kind, agent_id)`.
        let lk = lhs.kind().as_str();
        let rk = rhs.kind().as_str();
        match lk.cmp(rk) {
            Ordering::Less => Ordering::Less,
            Ordering::Greater => Ordering::Greater,
            Ordering::Equal => match (lhs, rhs) {
                (Self::Agent { agent_id: a }, Self::Agent { agent_id: b }) => {
                    a.as_uuid().cmp(&b.as_uuid())
                }
                _ => Ordering::Equal,
            },
        }
    }
}

impl fmt::Display for Participant {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Human => f.write_str("human"),
            Self::Agent { agent_id } => write!(f, "agent({agent_id})"),
            Self::System => f.write_str("system"),
        }
    }
}

crate::str_enum! {
    /// Tag-only side of [`Participant`]. The single source of truth for the
    /// `*_kind` column `CHECK` constraint, the JSON `kind` discriminator on
    /// [`Participant`], and any future tracing attribute (`patom.participant.kind`).
    pub enum ParticipantKind {
        Human  => "human",
        Agent  => "agent",
        System => "system",
    }
}

/// Sender of a `session_messages` row.
///
/// Wider than [`Participant`] because the worker injects `System` rows (the
/// ping-pong nudge "you produced text without calling send_message"). `System`
/// rows are never receivers and never appear in `sessions`'s participant
/// columns — that's why a separate type exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum MessageSender {
    Human,
    Agent {
        agent_id: AgentId,
    },
    /// Worker-injected nudge, surfaced to the receiving agent as a system note.
    System,
}

impl MessageSender {
    /// Promote a participant into a sender. Lossless — a participant can always
    /// send (System "sends" only via worker-injected nudges; the variant maps
    /// for completeness even though no agent-loop path constructs it).
    #[must_use]
    pub const fn from_participant(p: Participant) -> Self {
        match p {
            Participant::Human => Self::Human,
            Participant::Agent { agent_id } => Self::Agent { agent_id },
            Participant::System => Self::System,
        }
    }

    /// Tag without payload — value persisted in `session_messages.sender_kind`.
    #[must_use]
    pub const fn kind(self) -> MessageSenderKind {
        match self {
            Self::Human => MessageSenderKind::Human,
            Self::Agent { .. } => MessageSenderKind::Agent,
            Self::System => MessageSenderKind::System,
        }
    }

    /// Reconstruct a `MessageSender` from the column pair.
    ///
    /// # Errors
    ///
    /// Returns `Err` if the (kind, agent_id) pair violates the
    /// `session_messages.sender_kind = 'agent' iff sender_agent_id IS NOT NULL`
    /// invariant — caller observed an impossible row that must be reported as a
    /// backend error per CLAUDE.md §6.
    pub const fn from_kind_id(
        kind: MessageSenderKind,
        agent_id: Option<AgentId>,
    ) -> Result<Self, ParticipantDecodeError> {
        match (kind, agent_id) {
            (MessageSenderKind::Human, None) => Ok(Self::Human),
            (MessageSenderKind::System, None) => Ok(Self::System),
            (MessageSenderKind::Agent, Some(id)) => Ok(Self::Agent { agent_id: id }),
            _ => Err(ParticipantDecodeError::KindAgentMismatch),
        }
    }

    /// `Some(id)` for the agent variant; `None` for human or system.
    #[must_use]
    pub const fn agent_id(self) -> Option<AgentId> {
        match self {
            Self::Agent { agent_id } => Some(agent_id),
            Self::Human | Self::System => None,
        }
    }
}

crate::str_enum! {
    /// Tag-only side of [`MessageSender`]. Single source of truth for the
    /// `session_messages.sender_kind` column `CHECK` constraint.
    pub enum MessageSenderKind {
        Human  => "human",
        Agent  => "agent",
        System => "system",
    }
}

impl Participant {
    /// Reconstruct a `Participant` from the column pair stored in `sessions`
    /// (or `session_messages.receiver_*`).
    ///
    /// # Errors
    ///
    /// Returns `Err` when the `(kind, agent_id)` pair is inconsistent with
    /// the `(*_kind = 'agent') = (*_agent_id IS NOT NULL)` invariant. This
    /// can only happen if the row was hand-edited or a previous deploy used
    /// a stale serialiser — surface it rather than coerce.
    pub const fn from_kind_id(
        kind: ParticipantKind,
        agent_id: Option<AgentId>,
    ) -> Result<Self, ParticipantDecodeError> {
        match (kind, agent_id) {
            (ParticipantKind::Human, None) => Ok(Self::Human),
            (ParticipantKind::System, None) => Ok(Self::System),
            (ParticipantKind::Agent, Some(id)) => Ok(Self::Agent { agent_id: id }),
            _ => Err(ParticipantDecodeError::KindAgentMismatch),
        }
    }
}

/// Backend-invariant violation observed at decode time.
///
/// Constructible only by `from_kind_id` impls in this module; if a caller ever
/// sees one, it means the schema CHECK constraint failed to fire. CLAUDE.md
/// §6 — surface as a backend error rather than coerce silently.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ParticipantDecodeError {
    #[error("invariant: kind/agent_id mismatch")]
    KindAgentMismatch,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agent(n: u128) -> Participant {
        Participant::agent(AgentId::from(uuid::Uuid::from_u128(n)))
    }

    #[test]
    fn kind_round_trips_via_str() {
        assert_eq!(ParticipantKind::Human.as_str(), "human");
        assert_eq!(ParticipantKind::Agent.as_str(), "agent");
        assert_eq!(ParticipantKind::System.as_str(), "system");
        assert_eq!(
            ParticipantKind::parse("human"),
            Some(ParticipantKind::Human)
        );
        assert_eq!(
            ParticipantKind::parse("agent"),
            Some(ParticipantKind::Agent)
        );
        assert_eq!(
            ParticipantKind::parse("system"),
            Some(ParticipantKind::System)
        );
        assert_eq!(ParticipantKind::parse("nope"), None);
    }

    #[test]
    fn canonical_pair_orders_agent_before_system() {
        let s = Participant::system();
        let a = agent(1);
        assert_eq!(Participant::canonical_pair(a, s), Some((a, s)));
        assert_eq!(Participant::canonical_pair(s, a), Some((a, s)));
    }

    #[test]
    fn canonical_pair_orders_human_before_system() {
        let h = Participant::human();
        let s = Participant::system();
        assert_eq!(Participant::canonical_pair(h, s), Some((h, s)));
        assert_eq!(Participant::canonical_pair(s, h), Some((h, s)));
    }

    #[test]
    fn canonical_pair_rejects_two_systems() {
        let s = Participant::system();
        assert_eq!(Participant::canonical_pair(s, s), None);
    }

    #[test]
    fn kind_matches_participant_variant() {
        assert_eq!(Participant::human().kind(), ParticipantKind::Human);
        assert_eq!(agent(1).kind(), ParticipantKind::Agent);
    }

    #[test]
    fn agent_id_extractor_reflects_variant() {
        assert!(Participant::human().agent_id().is_none());
        let a = agent(7);
        assert_eq!(a.agent_id(), Some(AgentId::from(uuid::Uuid::from_u128(7))));
    }

    #[test]
    fn canonical_pair_orders_agent_before_human() {
        // SQL string-compare on participant_*_kind makes 'agent' < 'human';
        // Rust matches so the CHECK constraint
        // `sessions_participants_distinct` and the canonical upsert agree.
        let h = Participant::human();
        let a = agent(1);
        assert_eq!(Participant::canonical_pair(h, a), Some((a, h)));
        assert_eq!(Participant::canonical_pair(a, h), Some((a, h)));
    }

    #[test]
    fn canonical_pair_orders_agents_by_uuid() {
        let a = agent(2);
        let b = agent(5);
        assert_eq!(Participant::canonical_pair(a, b), Some((a, b)));
        assert_eq!(Participant::canonical_pair(b, a), Some((a, b)));
    }

    #[test]
    fn canonical_pair_rejects_equal() {
        let h = Participant::human();
        assert_eq!(Participant::canonical_pair(h, h), None);
        let a = agent(1);
        assert_eq!(Participant::canonical_pair(a, a), None);
    }

    #[test]
    fn serde_round_trip_human() {
        let h = Participant::human();
        let s = serde_json::to_string(&h).expect("serialize");
        assert_eq!(s, r#"{"kind":"human"}"#);
        let back: Participant = serde_json::from_str(&s).expect("deserialize");
        assert_eq!(back, h);
    }

    #[test]
    fn serde_round_trip_agent() {
        let id = AgentId::from(uuid::Uuid::from_u128(0xab_cd_ef));
        let a = Participant::agent(id);
        let s = serde_json::to_string(&a).expect("serialize");
        let back: Participant = serde_json::from_str(&s).expect("deserialize");
        assert_eq!(back, a);
    }

    #[test]
    fn display_is_stable() {
        assert_eq!(Participant::human().to_string(), "human");
        let a = agent(1);
        // Display starts with "agent(" — exact uuid form is not pinned here.
        assert!(a.to_string().starts_with("agent("));
    }
}
