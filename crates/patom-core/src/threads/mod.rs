//! Thread-feed chat store — the multi-participant successor to the 2-party
//! `session` model.
//!
//! A thread is one ordered feed; an agent's "session" is its
//! `(thread_id, agent_id)` participation ([`AgentThreadId`]).

mod error;
mod pg_store;
mod traits;

pub use error::ThreadError;
pub use pg_store::PgThreadStore;
pub use traits::{
    AgentThreadId, MessageKind, NewMessage, SharedThreadStore, ThreadId, ThreadMessageId,
    ThreadStore,
};
