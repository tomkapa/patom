//! Agent runtime.
//!
//! The execution layer the HTTP boundary calls into: request queue, lease management,
//! worker pool, and response (SSE) delivery. Traits are the seam — the Postgres impls
//! ([`PgPromptQueue`], [`PgResponseHub`]) sit behind them today; future backends drop
//! in the same way. See `doc/task1.md` for the full design.

pub mod dag;
pub mod error;
pub mod limits;
pub mod pg_queue;
pub mod pg_response;
pub mod pg_thread_stream;
pub mod queue;
pub mod response;
pub mod types;
pub mod worker;

pub use dag::{BudgetBumped, DagBudget, PgDagBudget, SharedDagBudget};
pub use error::{LeaseTimingError, PromptError, ResponseError};
pub use limits::{
    CANCEL_POLL_INTERVAL, CONTEXT_SUMMARY_MAX_BYTES, DEFAULT_THREAD_HISTORY_LIMIT,
    DEFAULT_THREAD_LIST_LIMIT, LEASE_HEARTBEAT_INTERVAL, LEASE_TTL, MAX_ATTEMPTS,
    MAX_CHUNK_BUFFER_PER_REQUEST, MAX_DAG_TURNS, MAX_GET_SESSION_LIMIT, MAX_PENDING_PER_SESSION,
    MAX_PINGPONG_RETRIES, MAX_PROMPT_BYTES, MAX_THREAD_CHUNK_BUFFER, MAX_THREAD_HISTORY_LIMIT,
    MAX_THREAD_LIST_LIMIT, MAX_THREAD_SLOTS, MAX_TURN_DURATION, MAX_WORKERS, THREAD_NOTIFY_CHANNEL,
    THREAD_PREVIEW_MAX_CHARS,
};
pub use pg_queue::PgPromptQueue;
pub use pg_response::PgResponseHub;
pub use pg_thread_stream::{
    PgThreadStream, SharedThreadStream, ThreadStream, ThreadStreamError, ThreadStreamEvent,
    ThreadStreamItem,
};
pub use queue::{
    ClaimReceipt, ClaimedPrompt, ClaimedSession, EnqueueOutcome, LeaseManager, LeaseTiming,
    LeaseToken, NewPromptRequest, PromptQueue, RequestStatusView, SharedLeaseManager,
    SharedPromptQueue,
};
pub use response::{
    ResponseChunk, ResponseChunkEnvelope, ResponseSink, ResponseSource, SharedResponseSink,
    SharedResponseSource, StreamEvent,
};
pub use types::{
    Attempts, ChunkSeq, FailureReason, IdempotencyKey, PromptRequestId, RequestKind,
    RequestKindPayload, RequestStatus, SeqOverflow, TurnSeq, WorkerId,
};
pub use worker::{WorkerConfig, WorkerPool, WorkerPoolHandle};
