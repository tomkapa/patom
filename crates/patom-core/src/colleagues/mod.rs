//! Colleagues directory — humans and agents as one addressable roster.
//!
//! Every member of an org, human or agent, has a `colleagues` row: a stable
//! [`ColleagueId`], a [`ColleagueKind`] discriminator, a display name, and a
//! satellite FK to the backing `users` / `agents` row. A session-end references
//! a colleague; the agent's roster block and `send_message` address peers by
//! colleague, so a human coworker and a peer agent are perceived as the same
//! kind of thing.
//!
//! The synthetic system end of a reflection/resolution session is **not** a
//! colleague — it is encoded as a NULL colleague reference (see
//! [`crate::types::Participant`]), so no `system` row or kind exists.
//!
//! This module owns the directory *vocabulary* (Stage 1). The store, cache, and
//! schema land in later stages.

mod cache;
mod error;
mod limits;
mod overrides;
mod pg_store;
mod profile;
mod render;
mod store;
mod types;

pub use cache::ColleagueRosterCache;
pub use error::ColleagueError;
pub use limits::{
    COLLEAGUE_NAME_MAX_LEN, COLLEAGUE_ROSTER_CACHE_CAP, COLLEAGUE_ROSTER_CACHE_TTL,
    COLLEAGUE_ROSTER_FETCH_MAX, MAX_ROSTER_INLINE,
};
pub use overrides::{NoThreadDisplayNames, SharedThreadDisplayNames, ThreadDisplayNames};
pub use pg_store::{PgColleagueStore, resolve_agent_colleague, resolve_user_colleague};
pub use profile::{
    ColleagueMatch, ColleagueProfile, DEFAULT_SEARCH_COLLEAGUE_K, Expertise, MAX_EXPERTISE,
    MAX_NOTES_PER_PARTICIPANT, MAX_PARTICIPANTS_INLINE, MAX_PREFERENCES, MAX_PROFILE_FETCH,
    MAX_PROFILE_TEXT, MAX_ROLE, PARTICIPANTS_TAG_CLOSE, PARTICIPANTS_TAG_OPEN, PROFILE_SNIPPET_LEN,
    ParticipantLine, ParticipantNote, ParticipantNotes, ParticipantSnippet, PgProfileStore,
    Preferences, ProfileError, ProfileStore, ProfileText, Role, SEARCH_COLLEAGUE_K,
    SharedProfileStore, profile_snippet, render_participants_block,
};
pub use render::{
    ROSTER_TAG_CLOSE, ROSTER_TAG_OPEN, SPEAKING_WITH_TAG_CLOSE, SPEAKING_WITH_TAG_OPEN,
    render_roster_block, render_speaking_with,
};
pub use store::{ColleagueStore, SharedColleagueStore};
pub use types::{Colleague, ColleagueId, ColleagueKind, ColleagueName, ColleagueRef};
