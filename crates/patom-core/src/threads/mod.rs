//! Thread-feed chat store — the multi-participant successor to the 2-party
//! `session` model.
//!
//! A thread is one ordered feed; an agent's "session" is its
//! `(thread_id, agent_id)` participation ([`AgentThreadId`]).

mod error;
mod limits;
mod pg_store;
mod traits;

pub use error::ThreadError;
pub use limits::{DEFAULT_THREAD_FEED, MAX_THREAD_FEED, MAX_THREAD_LIST};
pub use pg_store::PgThreadStore;
pub use traits::{
    AgentThreadId, FeedMessage, MessageKind, NewMessage, SharedThreadStore, ThreadId,
    ThreadListItem, ThreadMessageId, ThreadStore,
};
