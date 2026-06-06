use std::time::Duration;

use thiserror::Error;

use crate::session::{SessionError, SessionId};
use crate::types::ParseError;

use super::types::{PromptRequestId, SeqOverflow};

/// Construction-time error for [`crate::runtime::queue::LeaseTiming`]. Surfaces
/// at startup so a misconfigured deployment fails before serving traffic.
#[derive(Debug, Error)]
pub enum LeaseTimingError {
    #[error("heartbeat interval must be > 0")]
    IntervalZero,
    #[error(
        "heartbeat interval ({heartbeat_interval:?}) must be strictly less than \
         lease ttl ({ttl:?}); otherwise the lease silently expires between beats"
    )]
    IntervalNotUnderTtl {
        ttl: Duration,
        heartbeat_interval: Duration,
    },
}

#[derive(Debug, Error)]
pub enum PromptError {
    #[error("session: {0}")]
    Session(#[from] SessionError),

    #[error("parse: {0}")]
    Parse(#[from] ParseError),

    #[error("session {session:?} has reached its pending cap of {max}")]
    PendingCapExceeded { session: SessionId, max: u32 },

    #[error("request {0:?} not found")]
    RequestNotFound(PromptRequestId),

    #[error("session {0:?} not found")]
    SessionNotFound(SessionId),

    /// `enqueue` was called with `sender == Participant::agent(receiver_agent_id)`
    /// — a self-session has no representational shape. Caller should not have
    /// reached the queue with this combination.
    #[error("self-session: sender equals receiver")]
    SelfSession,

    /// [`crate::runtime::DagBudget::bump_or_fail`] observed `turns_used >=
    /// turns_cap`. Caller (the `send_message` tool) rolls its insert back so
    /// the new message disappears with the rejection.
    #[error("dag {root:?} budget exceeded: {turns_used}/{turns_cap}")]
    DagBudgetExceeded {
        root: PromptRequestId,
        turns_used: u32,
        turns_cap: u32,
    },

    /// [`crate::runtime::DagBudget::bump_or_fail`] could not find the dag row
    /// for `root`. Distinct from `DagBudgetExceeded` so a missing row (caller
    /// error) does not look like a normal budget rejection.
    #[error("dag {0:?} not found")]
    DagNotFound(PromptRequestId),

    #[error("lease for session {session:?} expired or superseded")]
    LeaseStale { session: SessionId },

    #[error("sequence: {0}")]
    Sequence(#[from] SeqOverflow),

    #[error("queue backend error: {0}")]
    Backend(String),

    #[error("queue db error: {0}")]
    Db(#[from] sqlx::Error),
}

/// Failure modes for response delivery — distinct from queue errors so HTTP can map
/// each cleanly to a status code.
#[derive(Debug, Error)]
pub enum ResponseError {
    #[error("request {0:?} has no active or persisted stream")]
    NotFound(PromptRequestId),

    #[error("subscriber lagged past the chunk buffer; reconnect with Last-Event-ID")]
    Lagged,

    #[error("sequence: {0}")]
    Sequence(#[from] SeqOverflow),

    #[error("response sink backend error: {0}")]
    Backend(String),

    #[error("response sink db error: {0}")]
    Db(#[from] sqlx::Error),
}
