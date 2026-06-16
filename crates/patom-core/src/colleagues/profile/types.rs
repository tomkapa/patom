//! Domain types for the colleague profile board (issue #183).
//!
//! A **profile** is the org-shared "who they are" record for one colleague:
//! durable role / expertise / preferences that any agent in the org can read
//! (the prompt's `<participants>` block) or search (`search_colleague`). This is
//! distinct from a private `collaborator` memory ("what *I* learned working with
//! them"), which stays per-agent in `agent_memories`.
//!
//! CLAUDE.md §1: each field carrying a length invariant is a newtype with a
//! `TryFrom` smart constructor that mirrors the `colleague_profiles` column
//! `CHECK`s (migration 79). [`ProfileText`] is *derived*, never user-set — it is
//! the flattened embedding source composed by [`compose_profile_text`].

use std::fmt;
use std::sync::Arc;

use crate::colleagues::{ColleagueId, ColleagueKind, ColleagueName};
use crate::types::ParseError;

use super::limits::{MAX_EXPERTISE, MAX_PREFERENCES, MAX_PROFILE_TEXT, MAX_ROLE};

/// Validated, non-empty job role (≤ [`MAX_ROLE`] bytes), e.g. "Product Manager".
#[derive(Clone, PartialEq, Eq)]
pub struct Role(Arc<str>);

/// Validated, non-empty free-text expertise (≤ [`MAX_EXPERTISE`] bytes).
#[derive(Clone, PartialEq, Eq)]
pub struct Expertise(Arc<str>);

/// Validated, non-empty free-text working preferences (≤ [`MAX_PREFERENCES`] bytes).
#[derive(Clone, PartialEq, Eq)]
pub struct Preferences(Arc<str>);

/// Composed embedding source (≤ [`MAX_PROFILE_TEXT`] bytes). Derived from the
/// structured fields by [`compose_profile_text`], not supplied by a caller.
#[derive(Clone, PartialEq, Eq)]
pub struct ProfileText(Arc<str>);

// Each bounded string follows the `AgentDescription` / `ColleagueName` shape
// (§1): empty-reject, byte-cap, `Arc<str>` so the value hands around without
// copying. Kept hand-written per `types/macros.rs`'s note that the shared
// macros deliberately exclude length-bounded string newtypes.
macro_rules! bounded_str {
    ($ty:ident, $field:literal, $cap:ident) => {
        impl $ty {
            #[must_use]
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl TryFrom<&str> for $ty {
            type Error = ParseError;

            fn try_from(raw: &str) -> Result<Self, Self::Error> {
                if raw.trim().is_empty() {
                    return Err(ParseError::Empty { field: $field });
                }
                if raw.len() > $cap {
                    return Err(ParseError::TooLong {
                        field: $field,
                        max: $cap,
                        got: raw.len(),
                    });
                }
                Ok(Self(Arc::from(raw)))
            }
        }

        impl TryFrom<String> for $ty {
            type Error = ParseError;
            fn try_from(raw: String) -> Result<Self, Self::Error> {
                Self::try_from(raw.as_str())
            }
        }

        impl fmt::Debug for $ty {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.debug_tuple(stringify!($ty)).field(&&*self.0).finish()
            }
        }
    };
}

bounded_str!(Role, "role", MAX_ROLE);
bounded_str!(Expertise, "expertise", MAX_EXPERTISE);
bounded_str!(Preferences, "preferences", MAX_PREFERENCES);
bounded_str!(ProfileText, "profile_text", MAX_PROFILE_TEXT);

/// An org-shared colleague profile.
///
/// Private fields per §1; readers only. At least one of role/expertise/
/// preferences is expected for a writable profile, but that is a *write-input*
/// rule enforced at the `profile_write` boundary (and surfaced by
/// [`compose_profile_text`] rejecting an all-empty composition), not a structural
/// invariant of the carrier — a read may legitimately reconstruct any shape the
/// row holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColleagueProfile {
    colleague_id: ColleagueId,
    role: Option<Role>,
    expertise: Option<Expertise>,
    preferences: Option<Preferences>,
    updated_by: Option<ColleagueId>,
}

impl ColleagueProfile {
    #[must_use]
    pub fn new(
        colleague_id: ColleagueId,
        role: Option<Role>,
        expertise: Option<Expertise>,
        preferences: Option<Preferences>,
        updated_by: Option<ColleagueId>,
    ) -> Self {
        Self {
            colleague_id,
            role,
            expertise,
            preferences,
            updated_by,
        }
    }

    #[must_use]
    pub fn colleague_id(&self) -> ColleagueId {
        self.colleague_id
    }

    #[must_use]
    pub fn role(&self) -> Option<&Role> {
        self.role.as_ref()
    }

    #[must_use]
    pub fn expertise(&self) -> Option<&Expertise> {
        self.expertise.as_ref()
    }

    #[must_use]
    pub fn preferences(&self) -> Option<&Preferences> {
        self.preferences.as_ref()
    }

    #[must_use]
    pub fn updated_by(&self) -> Option<ColleagueId> {
        self.updated_by
    }
}

/// One ranked hit from `search_colleague` — a colleague the agent can act on
/// directly (e.g. pull into a thread via `send_message { to }`).
///
/// Public fields mirror [`crate::colleagues::ColleagueRef`]: the values are
/// already-validated newtypes, so there is no further invariant to guard. The
/// `snippet` is a length-capped excerpt of the matched card / profile text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColleagueMatch {
    pub colleague_id: ColleagueId,
    pub kind: ColleagueKind,
    pub name: ColleagueName,
    pub snippet: String,
}

/// Flatten the present structured fields into the embedding source.
///
/// Joins one `Label: value` line per present field, in a stable order, so the
/// same profile always embeds to the same text (the re-embed skip in the store
/// relies on this determinism). Returns [`ParseError::Empty`] when no field is
/// present — an all-empty profile is not writable. The field caps guarantee the
/// join always fits [`MAX_PROFILE_TEXT`], so the only failure here is emptiness.
pub(super) fn compose_profile_text(p: &ColleagueProfile) -> Result<ProfileText, ParseError> {
    let mut parts: Vec<String> = Vec::with_capacity(3);
    if let Some(role) = p.role() {
        parts.push(format!("Role: {}", role.as_str()));
    }
    if let Some(expertise) = p.expertise() {
        parts.push(format!("Expertise: {}", expertise.as_str()));
    }
    if let Some(preferences) = p.preferences() {
        parts.push(format!("Prefers: {}", preferences.as_str()));
    }
    ProfileText::try_from(parts.join("\n").as_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn role(s: &str) -> Role {
        Role::try_from(s).expect("valid role")
    }

    #[test]
    fn role_rejects_empty() {
        let err = Role::try_from("   ").expect_err("blank must reject");
        assert!(matches!(err, ParseError::Empty { field: "role" }));
    }

    #[test]
    fn role_rejects_over_cap() {
        let long = "x".repeat(MAX_ROLE + 1);
        let err = Role::try_from(long.as_str()).expect_err("over cap must reject");
        assert!(matches!(
            err,
            ParseError::TooLong {
                field: "role",
                max: MAX_ROLE,
                ..
            }
        ));
    }

    #[test]
    fn role_accepts_at_cap() {
        let at = "x".repeat(MAX_ROLE);
        let parsed = Role::try_from(at.as_str()).expect("cap-length is valid");
        assert_eq!(parsed.as_str().len(), MAX_ROLE);
    }

    #[test]
    fn expertise_and_preferences_share_their_caps() {
        assert!(Expertise::try_from("x".repeat(MAX_EXPERTISE).as_str()).is_ok());
        assert!(Expertise::try_from("x".repeat(MAX_EXPERTISE + 1).as_str()).is_err());
        assert!(Preferences::try_from("x".repeat(MAX_PREFERENCES).as_str()).is_ok());
        assert!(Preferences::try_from("x".repeat(MAX_PREFERENCES + 1).as_str()).is_err());
    }

    #[test]
    fn compose_joins_present_fields_in_stable_order() {
        let p = ColleagueProfile::new(
            ColleagueId::new(),
            Some(role("Product Manager")),
            Some(Expertise::try_from("billing, pricing").expect("valid")),
            Some(Preferences::try_from("async-first").expect("valid")),
            None,
        );
        let text = compose_profile_text(&p).expect("non-empty composes");
        assert_eq!(
            text.as_str(),
            "Role: Product Manager\nExpertise: billing, pricing\nPrefers: async-first"
        );
    }

    #[test]
    fn compose_skips_absent_fields() {
        let p = ColleagueProfile::new(ColleagueId::new(), Some(role("Designer")), None, None, None);
        let text = compose_profile_text(&p).expect("one field is enough");
        assert_eq!(text.as_str(), "Role: Designer");
    }

    #[test]
    fn compose_rejects_all_empty() {
        let p = ColleagueProfile::new(ColleagueId::new(), None, None, None, None);
        let err = compose_profile_text(&p).expect_err("all-empty is not writable");
        assert!(matches!(
            err,
            ParseError::Empty {
                field: "profile_text"
            }
        ));
    }

    #[test]
    fn readers_round_trip() {
        let id = ColleagueId::new();
        let by = ColleagueId::new();
        let p = ColleagueProfile::new(id, Some(role("PM")), None, None, Some(by));
        assert_eq!(p.colleague_id(), id);
        assert_eq!(p.role().expect("set").as_str(), "PM");
        assert!(p.expertise().is_none());
        assert_eq!(p.updated_by(), Some(by));
    }
}
