//! Domain types for the colleagues directory.
//!
//! A **colleague** is one addressable end of a session: a distinct, named
//! identity backed by either a human ([`crate::auth::UserId`]) or an agent
//! ([`crate::agents::AgentId`]). The agent perceives both as the same kind of
//! thing — a named coworker it can address and remember — while only agents
//! execute turns.
//!
//! CLAUDE.md §1: every value carrying an invariant gets a newtype with a
//! `TryFrom` smart constructor. The kind ⇔ satellite invariant (a `Human`
//! carries a `user_id`, an `Agent` carries an `agent_id`) is parsed exactly
//! once, here, via `TryFrom<ColleagueRow>` — the same invariant the
//! `colleagues_kind_satellite` column `CHECK` enforces in the database.
//!
//! There is no `System` colleague: the synthetic system end of a
//! reflection/resolution session is encoded as a *NULL* colleague reference,
//! not a row (see [`crate::types::Participant`]). So [`ColleagueKind`] is
//! `human | agent` only.

use std::fmt;
use std::sync::Arc;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use uuid::Uuid;

use crate::agents::AgentId;
use crate::auth::{OrgId, UserId};
use crate::types::{ParseError, Participant};

use super::error::ColleagueError;
use super::limits::COLLEAGUE_NAME_MAX_LEN;

crate::uuid_newtype! {
    /// Opaque identifier for a `colleagues` row — one per `(org, human)` and
    /// per `(org, agent)`. The model addresses peers by this id in
    /// `send_message`; sessions reference it as a participant.
    pub ColleagueId
}

impl TryFrom<&str> for ColleagueId {
    type Error = ParseError;

    /// Boundary parse for the tool/HTTP surface where a colleague id arrives as
    /// a string (§1). DB-sourced ids decode straight through the macro's sqlx
    /// `Decode`, so this path is only for untrusted text.
    fn try_from(raw: &str) -> Result<Self, Self::Error> {
        Uuid::parse_str(raw)
            .map(Self::from)
            .map_err(|_| ParseError::Malformed {
                field: "colleague_id",
                detail: "expected uuid",
            })
    }
}

crate::str_enum! {
    /// What backs a colleague. Single source of truth for the
    /// `colleagues.kind` column `CHECK ('human','agent')`, the JSON tag, and
    /// the `patom.colleague.kind` tracing attribute. No `system` — System is
    /// the NULL-reference convention, not a colleague row.
    pub enum ColleagueKind {
        Human => "human",
        Agent => "agent",
    }
}

/// Validated, non-empty colleague display name (≤ [`COLLEAGUE_NAME_MAX_LEN`] bytes).
///
/// Reference-counted so the roster renderer can hand the same allocation
/// around without copying. Mirrors `AgentName`'s shape (§1).
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct ColleagueName(Arc<str>);

impl ColleagueName {
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<&str> for ColleagueName {
    type Error = ParseError;

    fn try_from(raw: &str) -> Result<Self, Self::Error> {
        if raw.is_empty() {
            return Err(ParseError::Empty {
                field: "colleague_name",
            });
        }
        if raw.len() > COLLEAGUE_NAME_MAX_LEN {
            return Err(ParseError::TooLong {
                field: "colleague_name",
                max: COLLEAGUE_NAME_MAX_LEN,
                got: raw.len(),
            });
        }
        Ok(Self(Arc::from(raw)))
    }
}

impl TryFrom<String> for ColleagueName {
    type Error = ParseError;
    fn try_from(raw: String) -> Result<Self, Self::Error> {
        Self::try_from(raw.as_str())
    }
}

impl fmt::Debug for ColleagueName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("ColleagueName").field(&&*self.0).finish()
    }
}

impl fmt::Display for ColleagueName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl Serialize for ColleagueName {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for ColleagueName {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        Self::try_from(raw).map_err(serde::de::Error::custom)
    }
}

/// Slim, model-facing handle for a colleague.
///
/// What the roster block and tool I/O surface — identity + label without the
/// satellite FKs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ColleagueRef {
    pub id: ColleagueId,
    pub kind: ColleagueKind,
    pub display_name: ColleagueName,
}

/// A fully-resolved colleague.
///
/// Private fields per §1: the kind ⇔ satellite invariant is established at
/// construction by [`Self::try_new`] and the `colleagues_kind_satellite` DB
/// CHECK; readers go through the accessors so the pairing rule can never be
/// dodged by callers building a `Self { ... }` struct literal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Colleague {
    id: ColleagueId,
    org_id: OrgId,
    kind: ColleagueKind,
    display_name: ColleagueName,
    user_id: Option<UserId>,
    agent_id: Option<AgentId>,
}

impl Colleague {
    /// The one boundary at which a `Colleague` enters the typed world (§1).
    ///
    /// Enforces the kind ⇔ satellite invariant: `Human` carries a `user_id`
    /// with no `agent_id`, `Agent` carries an `agent_id` with no `user_id`.
    /// This duplicates the DB `colleagues_kind_satellite` CHECK as
    /// after-read defence-in-depth per §6 — observing a violation means
    /// schema and code disagree.
    pub fn try_new(
        id: ColleagueId,
        org_id: OrgId,
        kind: ColleagueKind,
        display_name: ColleagueName,
        user_id: Option<UserId>,
        agent_id: Option<AgentId>,
    ) -> Result<Self, ColleagueError> {
        let consistent = match kind {
            ColleagueKind::Human => user_id.is_some() && agent_id.is_none(),
            ColleagueKind::Agent => agent_id.is_some() && user_id.is_none(),
        };
        if !consistent {
            return Err(ColleagueError::KindSatelliteMismatch);
        }
        Ok(Self {
            id,
            org_id,
            kind,
            display_name,
            user_id,
            agent_id,
        })
    }

    #[must_use]
    pub fn id(&self) -> ColleagueId {
        self.id
    }

    #[must_use]
    pub fn org_id(&self) -> OrgId {
        self.org_id
    }

    #[must_use]
    pub fn kind(&self) -> ColleagueKind {
        self.kind
    }

    #[must_use]
    pub fn display_name(&self) -> &ColleagueName {
        &self.display_name
    }

    /// `Some(user_id)` iff this colleague is human-backed.
    #[must_use]
    pub fn user_id(&self) -> Option<UserId> {
        self.user_id
    }

    /// `Some(agent_id)` iff this colleague is agent-backed.
    #[must_use]
    pub fn agent_id(&self) -> Option<AgentId> {
        self.agent_id
    }

    /// Project to the slim model-facing handle.
    #[must_use]
    pub fn to_ref(&self) -> ColleagueRef {
        ColleagueRef {
            id: self.id,
            kind: self.kind,
            display_name: self.display_name.clone(),
        }
    }
}

/// Project a fully-resolved colleague onto the addressing-layer
/// [`Participant`].
///
/// Total by construction: the kind ⇔ satellite invariant established by
/// [`Colleague::try_new`] guarantees the matching satellite id is present, so a
/// missing id here means the invariant was bypassed — an assertion failure
/// (§6), not a recoverable error. Callers that hold a `Colleague` get the
/// projection via `.into()` rather than re-matching `kind` themselves.
impl From<&Colleague> for Participant {
    fn from(c: &Colleague) -> Self {
        match c.kind {
            ColleagueKind::Human => Self::human(
                c.id,
                c.user_id
                    .expect("invariant: human colleague carries user_id"),
            ),
            ColleagueKind::Agent => Self::agent(
                c.id,
                c.agent_id
                    .expect("invariant: agent colleague carries agent_id"),
            ),
        }
    }
}

// The previous `TryFrom<ColleagueRow> for Colleague` was a one-use indirection:
// the store decoded into a raw `ColleagueRow`, then field-copied via `TryFrom`.
// The raw shape never escaped the store and the conversion's signature was
// identical to `Colleague::try_new`'s, so the intermediate type added no value.

#[cfg(test)]
mod tests {
    use super::*;

    fn name() -> ColleagueName {
        ColleagueName::try_from("Tom").expect("valid name")
    }

    fn try_human(
        user_id: Option<UserId>,
        agent_id: Option<AgentId>,
    ) -> Result<Colleague, ColleagueError> {
        Colleague::try_new(
            ColleagueId::new(),
            OrgId::new(),
            ColleagueKind::Human,
            name(),
            user_id,
            agent_id,
        )
    }

    fn try_agent(
        user_id: Option<UserId>,
        agent_id: Option<AgentId>,
    ) -> Result<Colleague, ColleagueError> {
        Colleague::try_new(
            ColleagueId::new(),
            OrgId::new(),
            ColleagueKind::Agent,
            name(),
            user_id,
            agent_id,
        )
    }

    #[test]
    fn colleague_id_round_trips_via_try_from() {
        let u = Uuid::from_u128(0x1234_5678);
        let id = ColleagueId::try_from(u.to_string().as_str()).expect("parse uuid");
        assert_eq!(id.as_uuid(), u);
    }

    #[test]
    fn colleague_id_rejects_malformed() {
        let err = ColleagueId::try_from("not-a-uuid").expect_err("must reject");
        assert!(matches!(
            err,
            ParseError::Malformed {
                field: "colleague_id",
                ..
            }
        ));
    }

    #[test]
    fn colleague_kind_round_trips_every_variant() {
        for k in ColleagueKind::ALL.iter().copied() {
            assert_eq!(ColleagueKind::parse(k.as_str()), Some(k));
        }
        assert_eq!(ColleagueKind::Human.as_str(), "human");
        assert_eq!(ColleagueKind::Agent.as_str(), "agent");
        // System is the NULL convention, never a kind label.
        assert_eq!(ColleagueKind::parse("system"), None);
    }

    #[test]
    fn colleague_name_rejects_empty() {
        let err = ColleagueName::try_from("").expect_err("must reject empty");
        assert!(matches!(err, ParseError::Empty { .. }));
    }

    #[test]
    fn try_new_accepts_consistent_human() {
        let c = try_human(Some(UserId::new()), None).expect("human is consistent");
        assert_eq!(c.kind(), ColleagueKind::Human);
        assert!(c.user_id().is_some());
        assert!(c.agent_id().is_none());
    }

    #[test]
    fn try_new_accepts_consistent_agent() {
        let c = try_agent(None, Some(AgentId::new())).expect("agent is consistent");
        assert_eq!(c.kind(), ColleagueKind::Agent);
        assert!(c.agent_id().is_some());
        assert!(c.user_id().is_none());
    }

    #[test]
    fn try_new_rejects_human_with_agent_satellite() {
        let err = try_human(None, Some(AgentId::new())).expect_err("mismatch must reject");
        assert!(matches!(err, ColleagueError::KindSatelliteMismatch));
    }

    #[test]
    fn try_new_rejects_agent_with_user_satellite() {
        let err = try_agent(Some(UserId::new()), None).expect_err("mismatch must reject");
        assert!(matches!(err, ColleagueError::KindSatelliteMismatch));
    }

    #[test]
    fn to_ref_preserves_identity() {
        let id = ColleagueId::new();
        let c = Colleague::try_new(
            id,
            OrgId::new(),
            ColleagueKind::Agent,
            name(),
            None,
            Some(AgentId::new()),
        )
        .expect("consistent");
        let r = c.to_ref();
        assert_eq!(r.id, id);
        assert_eq!(r.kind, ColleagueKind::Agent);
        assert_eq!(r.display_name.as_str(), "Tom");
    }
}
