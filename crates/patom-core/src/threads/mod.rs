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
pub use limits::{
    DEFAULT_THREAD_FEED, MAX_CONTEXT_MESSAGES, MAX_TAGS_PER_MESSAGE, MAX_THREAD_FEED,
    MAX_THREAD_LIST, MAX_TOOL_RESULT_CHARS, ROOT_SNIPPET_MAX_CHARS,
};
pub use pg_store::PgThreadStore;
pub use traits::{
    AgentThreadId, ContextTail, FeedMessage, MessageKind, NewMessage, RootSummary, Seq,
    SharedThreadStore, TailRow, ThreadCompaction, ThreadId, ThreadListItem, ThreadMessageId,
    ThreadParticipants, ThreadScope, ThreadStore,
};
