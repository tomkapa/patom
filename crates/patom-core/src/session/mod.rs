//! Session abstraction.
//!
//! A session owns the conversation history of a single agent interaction. The agent
//! never holds a `Vec<ChatMessage>` directly — it asks a `SessionStore` for a snapshot
//! before each turn and appends new messages back. The Postgres-backed
//! [`PgSessionStore`] is the only impl today; future backends (Redis, S3) plug in
//! behind the same trait.

mod error;
mod limits;
mod pg_store;
mod traits;

pub use error::SessionError;
pub use limits::MAX_MESSAGES_PER_SESSION;
pub use pg_store::PgSessionStore;
pub use traits::{SessionId, SessionStore, SessionTenancy, SharedSessionStore};
