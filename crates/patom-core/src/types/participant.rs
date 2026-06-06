//! Conversation participant — a colleague (human- or agent-backed) or the
//! synthetic system end.
//!
//! Multi-agent communication makes sessions strictly 2-party. Each session has
//! two participants drawn from this enum. CLAUDE.md §1 says to encode
//! invariants in types — `Participant` replaces three implicit conventions
//! (a nullable `agent_id`, a `role` string, and ad-hoc `is_human` checks)
//! with one closed sum.
//!
//! Storage shape (after migration 58): each end is `participant_*_colleague_id
//! UUID NULL REFERENCES colleagues(id)`. A NULL colleague reference encodes
//! [`Participant::System`] (the synthetic end of a reflection/resolution
//! session); the canonical-pair invariant keeps the real colleague in slot
//! `a` and System (if any) in slot `b`. Real colleagues compare by
//! `colleague_id` UUID; System sorts last. Two `Real` participants tie-break
//! by UUID; two `System`s are representationally invalid (a self-pair).
//!
//! The wire/serde shape stays kind-tagged: `{"kind":"human","colleague_id":
//! "...","user_id":"..."}`, `{"kind":"agent","colleague_id":"...","agent_id":
//! "..."}`, `{"kind":"system"}`. This keeps tracing attributes
//! (`patom.participant.kind`) and tooling stable; only the column shape
//! changed, not the JSON envelope.

use std::cmp::Ordering;
use std::fmt;

use serde::{Deserialize, Serialize};

use crate::agents::AgentId;
use crate::auth::UserId;
use crate::colleagues::ColleagueId;

/// One end of a session.
///
/// Constructed only via [`Self::human`] / [`Self::agent`] / [`Self::system`] —
/// there is no public field, so the only valid shapes are the three below.
/// `System` is the synthetic singleton counterpart used by autonomous
/// agent-only sessions (reflection, resolution per doc/memory.md §1.6, §1.8):
/// the agent talks to itself for audit; the System side never speaks back.
///
/// `Human` and `Agent` both carry a [`ColleagueId`] (their identity in the
/// org's directory) plus the satellite key (`UserId` / `AgentId`) that backs
/// it. Both are present together: the directory mints a colleague for every
/// agent and every membership, so a satellite always has a colleague and vice
/// versa.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Participant {
    Human {
        colleague_id: ColleagueId,
        user_id: UserId,
    },
    Agent {
        colleague_id: ColleagueId,
        agent_id: AgentId,
    },
    /// Synthetic counterpart for reflection / resolution sessions. The agent
    /// is paired with `System` so the canonical-pair invariant holds without
    /// relaxing it for self-sessions; nobody on this side speaks. Carries no
    /// id — System is the NULL-colleague convention, not a directory row.
    System,
}

impl Participant {
    /// A human colleague at one end of a session.
    #[must_use]
    pub const fn human(colleague_id: ColleagueId, user_id: UserId) -> Self {
        Self::Human {
            colleague_id,
            user_id,
        }
    }

    /// An agent colleague at one end of a session.
    #[must_use]
    pub const fn agent(colleague_id: ColleagueId, agent_id: AgentId) -> Self {
        Self::Agent {
            colleague_id,
            agent_id,
        }
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
            Self::Human { .. } => ParticipantKind::Human,
            Self::Agent { .. } => ParticipantKind::Agent,
            Self::System => ParticipantKind::System,
        }
    }

    /// `Some(id)` for `Human` and `Agent`; `None` for `System`. Drives the
    /// canonical-pair UUID ordering and the per-message self-detection in
    /// `map_message_for_viewer`.
    #[must_use]
    pub const fn colleague_id(self) -> Option<ColleagueId> {
        match self {
            Self::Human { colleague_id, .. } | Self::Agent { colleague_id, .. } => {
                Some(colleague_id)
            }
            Self::System => None,
        }
    }

    /// `Some(agent_id)` for the agent variant, `None` for human or system.
    #[must_use]
    pub const fn agent_id(self) -> Option<AgentId> {
        match self {
            Self::Agent { agent_id, .. } => Some(agent_id),
            Self::Human { .. } | Self::System => None,
        }
    }

    /// `Some(user_id)` for the human variant, `None` for agent or system.
    #[must_use]
    pub const fn user_id(self) -> Option<UserId> {
        match self {
            Self::Human { user_id, .. } => Some(user_id),
            Self::Agent { .. } | Self::System => None,
        }
    }

    /// True iff this is a human end.
    #[must_use]
    pub const fn is_human(self) -> bool {
        matches!(self, Self::Human { .. })
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
    /// Returns `None` if `lhs == rhs` (same colleague_id or two `System`s) —
    /// a self-session is representationally invalid; callers must reject
    /// before calling here.
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
    /// Mirrors the Postgres CHECK constraint `sessions_participants_distinct`
    /// in migration 58: real colleagues compare by `colleague_id` UUID
    /// (`participant_a_colleague_id < participant_b_colleague_id`); `System`
    /// always sorts last because it has no id and slot `b` is the only
    /// nullable end. Two `System`s compare equal — a self-pair the caller
    /// must reject.
    pub fn canonical_cmp(lhs: &Self, rhs: &Self) -> Ordering {
        match (lhs.colleague_id(), rhs.colleague_id()) {
            (Some(l), Some(r)) => l.as_uuid().cmp(&r.as_uuid()),
            // System (None) sorts last so the real colleague takes slot `a`
            // and the NULL ends up in `participant_b_colleague_id`.
            (Some(_), None) => Ordering::Less,
            (None, Some(_)) => Ordering::Greater,
            (None, None) => Ordering::Equal,
        }
    }
}

impl fmt::Display for Participant {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Human { colleague_id, .. } => write!(f, "human({colleague_id})"),
            Self::Agent { colleague_id, .. } => write!(f, "agent({colleague_id})"),
            Self::System => f.write_str("system"),
        }
    }
}

crate::str_enum! {
    /// Tag-only side of [`Participant`]. The single source of truth for the
    /// JSON `kind` discriminator on [`Participant`] and the
    /// `patom.participant.kind` tracing attribute. Storage no longer carries
    /// a kind column (the colleague_id FK plus the joined `colleagues.kind`
    /// is the source of truth), so this enum is now wire-only.
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
    Human {
        colleague_id: ColleagueId,
        user_id: UserId,
    },
    Agent {
        colleague_id: ColleagueId,
        agent_id: AgentId,
    },
    /// Worker-injected nudge, surfaced to the receiving agent as a system
    /// note. Stored as `sender_colleague_id IS NULL` (§Participant doc).
    System,
}

impl MessageSender {
    /// Promote a participant into a sender. Lossless — every `Participant`
    /// variant maps exactly to a `MessageSender` variant.
    #[must_use]
    pub const fn from_participant(p: Participant) -> Self {
        match p {
            Participant::Human {
                colleague_id,
                user_id,
            } => Self::Human {
                colleague_id,
                user_id,
            },
            Participant::Agent {
                colleague_id,
                agent_id,
            } => Self::Agent {
                colleague_id,
                agent_id,
            },
            Participant::System => Self::System,
        }
    }

    /// Tag-only kind — drives tracing and serde.
    #[must_use]
    pub const fn kind(self) -> MessageSenderKind {
        match self {
            Self::Human { .. } => MessageSenderKind::Human,
            Self::Agent { .. } => MessageSenderKind::Agent,
            Self::System => MessageSenderKind::System,
        }
    }

    /// `Some(colleague_id)` for `Human`/`Agent`; `None` for `System`.
    #[must_use]
    pub const fn colleague_id(self) -> Option<ColleagueId> {
        match self {
            Self::Human { colleague_id, .. } | Self::Agent { colleague_id, .. } => {
                Some(colleague_id)
            }
            Self::System => None,
        }
    }

    /// `Some(id)` for the agent variant; `None` for human or system.
    #[must_use]
    pub const fn agent_id(self) -> Option<AgentId> {
        match self {
            Self::Agent { agent_id, .. } => Some(agent_id),
            Self::Human { .. } | Self::System => None,
        }
    }

    /// `Some(id)` for the human variant; `None` for agent or system.
    #[must_use]
    pub const fn user_id(self) -> Option<UserId> {
        match self {
            Self::Human { user_id, .. } => Some(user_id),
            Self::Agent { .. } | Self::System => None,
        }
    }
}

crate::str_enum! {
    /// Tag-only side of [`MessageSender`] — drives tracing only.
    pub enum MessageSenderKind {
        Human  => "human",
        Agent  => "agent",
        System => "system",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    fn cid(n: u128) -> ColleagueId {
        ColleagueId::from(Uuid::from_u128(n))
    }

    fn human(n: u128) -> Participant {
        Participant::human(cid(n), UserId::from(Uuid::from_u128(n + 0xa000)))
    }

    fn agent(n: u128) -> Participant {
        Participant::agent(cid(n), AgentId::from(Uuid::from_u128(n + 0xb000)))
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
    fn canonical_pair_orders_real_colleague_before_system() {
        let s = Participant::system();
        let a = agent(1);
        assert_eq!(Participant::canonical_pair(a, s), Some((a, s)));
        assert_eq!(Participant::canonical_pair(s, a), Some((a, s)));
        let h = human(2);
        assert_eq!(Participant::canonical_pair(h, s), Some((h, s)));
        assert_eq!(Participant::canonical_pair(s, h), Some((h, s)));
    }

    #[test]
    fn canonical_pair_orders_real_colleagues_by_uuid() {
        // Two reals sort by colleague_id UUID — kind no longer participates
        // in the ordering. A human with the lower uuid takes slot a even
        // versus an agent with the higher uuid.
        let lower = human(1);
        let higher = agent(2);
        assert_eq!(Participant::canonical_pair(lower, higher), Some((lower, higher)));
        assert_eq!(Participant::canonical_pair(higher, lower), Some((lower, higher)));
    }

    #[test]
    fn canonical_pair_rejects_two_systems() {
        let s = Participant::system();
        assert_eq!(Participant::canonical_pair(s, s), None);
    }

    #[test]
    fn canonical_pair_rejects_same_colleague_id() {
        // Two participants sharing a colleague_id is a self-pair regardless
        // of satellite. The caller must reject before reaching the store.
        let a = agent(7);
        assert_eq!(Participant::canonical_pair(a, a), None);
        let h = human(7);
        let a_same_cid = Participant::agent(
            cid(7),
            AgentId::from(Uuid::from_u128(0xdead)),
        );
        assert_eq!(Participant::canonical_pair(h, a_same_cid), None);
    }

    #[test]
    fn kind_matches_participant_variant() {
        assert_eq!(human(1).kind(), ParticipantKind::Human);
        assert_eq!(agent(1).kind(), ParticipantKind::Agent);
        assert_eq!(Participant::system().kind(), ParticipantKind::System);
    }

    #[test]
    fn accessors_reflect_variant() {
        let h = human(3);
        assert_eq!(h.colleague_id(), Some(cid(3)));
        assert!(h.user_id().is_some());
        assert!(h.agent_id().is_none());
        let a = agent(4);
        assert_eq!(a.colleague_id(), Some(cid(4)));
        assert!(a.agent_id().is_some());
        assert!(a.user_id().is_none());
        let s = Participant::system();
        assert!(s.colleague_id().is_none());
        assert!(s.agent_id().is_none());
        assert!(s.user_id().is_none());
    }

    #[test]
    fn serde_round_trip_human() {
        let h = human(0xab);
        let s = serde_json::to_string(&h).expect("serialize");
        let back: Participant = serde_json::from_str(&s).expect("deserialize");
        assert_eq!(back, h);
    }

    #[test]
    fn serde_round_trip_agent_and_system() {
        let a = agent(0xcd);
        let s = serde_json::to_string(&a).expect("serialize");
        let back: Participant = serde_json::from_str(&s).expect("deserialize");
        assert_eq!(back, a);

        let sys = Participant::system();
        let raw = serde_json::to_string(&sys).expect("serialize sys");
        assert_eq!(raw, r#"{"kind":"system"}"#);
        let back: Participant = serde_json::from_str(&raw).expect("deserialize sys");
        assert_eq!(back, sys);
    }

    #[test]
    fn display_includes_colleague_id() {
        let h = human(1);
        assert!(h.to_string().starts_with("human("));
        let a = agent(1);
        assert!(a.to_string().starts_with("agent("));
        assert_eq!(Participant::system().to_string(), "system");
    }

    #[test]
    fn message_sender_from_participant_round_trips() {
        let h = human(1);
        let ms = MessageSender::from_participant(h);
        assert_eq!(ms.colleague_id(), h.colleague_id());
        assert_eq!(ms.user_id(), h.user_id());
        let s = MessageSender::from_participant(Participant::system());
        assert!(matches!(s, MessageSender::System));
    }
}
