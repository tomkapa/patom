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
mod pg_store;
mod store;
mod types;

pub use cache::ColleagueRosterCache;
pub use error::ColleagueError;
pub use limits::{
    COLLEAGUE_NAME_MAX_LEN, COLLEAGUE_ROSTER_CACHE_CAP, COLLEAGUE_ROSTER_CACHE_TTL,
    COLLEAGUE_ROSTER_FETCH_MAX,
};
pub use pg_store::{PgColleagueStore, resolve_agent_colleague, resolve_user_colleague};
pub use store::{ColleagueStore, SharedColleagueStore};
pub use types::{Colleague, ColleagueId, ColleagueKind, ColleagueName, ColleagueRef};
