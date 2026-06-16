//! Colleague profile board — the org-shared "who they are" record (issue #183).
//!
//! One [`ColleagueProfile`] per colleague: durable role / expertise / preferences
//! that any agent in the org can read (the prompt `<participants>` block) or find
//! (`search_colleague`). Distinct from a private `collaborator` memory, which
//! stays per-agent in `agent_memories`.

mod error;
mod limits;
mod pg_store;
mod store;
mod types;

pub use error::ProfileError;
pub use limits::{
    DEFAULT_SEARCH_COLLEAGUE_K, MAX_EXPERTISE, MAX_PREFERENCES, MAX_PROFILE_FETCH,
    MAX_PROFILE_TEXT, MAX_ROLE, PROFILE_SNIPPET_LEN, SEARCH_COLLEAGUE_K,
};
pub use pg_store::PgProfileStore;
pub use store::{ProfileStore, SharedProfileStore};
pub use types::{ColleagueMatch, ColleagueProfile, Expertise, Preferences, ProfileText, Role};
