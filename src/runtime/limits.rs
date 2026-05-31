//! Caps on the prompt pipeline. CLAUDE.md §5 — every value documented with *why this
//! number*; never magic numbers in logic.

use std::time::Duration;

/// Bounded `JoinSet` size for the worker pool.
///
/// Sized so a single host can sustain a reasonable steady-state concurrency without
/// exhausting the provider's per-tenant rate limit; raise via config when the
/// deployment scales horizontally.
pub const MAX_WORKERS: usize = 16;

/// Hard cap on simultaneously-pending prompts on a single session. A client storm
/// (re-tries, hot loops) cannot pin a session indefinitely; past this we refuse.
pub const MAX_PENDING_PER_SESSION: u32 = 32;

/// Outer fence on a single turn (provider call + tool calls + persistence). Above this
/// the worker abandons the turn so a stuck call can never hold a lease forever.
pub const MAX_TURN_DURATION: Duration = Duration::from_mins(3);

/// Lease lifetime granted at claim time. Sized so the heartbeat cadence
/// (`LEASE_TTL / 3`) still leaves >1 chance to renew before expiry under normal load.
pub const LEASE_TTL: Duration = Duration::from_secs(30);

/// Heartbeat cadence — exactly `LEASE_TTL / 3`. Two-thirds margin tolerates a missed
/// beat before another worker claims.
pub const LEASE_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(10);

const _: () = assert!(LEASE_HEARTBEAT_INTERVAL.as_secs() * 3 == LEASE_TTL.as_secs());

/// Maximum attempts per prompt. Past this, the prompt is parked as `failed` with
/// `reason = poison` so a request that reliably crashes the worker cannot pin a
/// session forever.
pub const MAX_ATTEMPTS: u32 = 3;

/// Broadcast channel buffer per request. Slow SSE clients hitting this bound get a
/// `Stalled` chunk and must reconnect with `Last-Event-ID` to catch up via the log.
pub const MAX_CHUNK_BUFFER_PER_REQUEST: usize = 256;

/// HTTP body cap for `POST /prompts`. Crossbar with `Prompt`'s own cap; the smaller
/// applies. Belt-and-braces so the boundary check happens before deserialisation.
pub const MAX_PROMPT_BYTES: usize = 64 * 1024;

/// Maximum byte length of an idempotency key. Long enough for a UUID + version
/// prefix; short enough that a misuse cannot blow query / index size.
pub const MAX_IDEMPOTENCY_KEY_BYTES: usize = 200;

/// Polling interval used by an idle worker between `claim_next_session` calls. Flat,
/// no backoff — when notify lands later this constant disappears.
pub const WORKER_IDLE_POLL: Duration = Duration::from_secs(1);

/// Polling cadence used by the per-claim cancel watcher inside the worker.
///
/// Trades a single inexpensive status read every interval for mid-turn
/// cancellation latency — at this cadence a `POST /requests/:id/cancel` is
/// observed within `CANCEL_POLL_INTERVAL` and fires the agent's
/// `CancellationToken`.
pub const CANCEL_POLL_INTERVAL: Duration = Duration::from_millis(200);

/// Total `send_message` calls permitted across one DAG.
///
/// One DAG is the causal tree rooted at a human's first request. Once this
/// cap is reached every further `send_message` rolls back with
/// `DagBudgetExceeded`. Sized to allow reasonable multi-agent collaboration
/// without runaway loops.
pub const MAX_DAG_TURNS: u32 = 64;

/// Worker-level retry budget for the ping-pong defence.
///
/// If an agent's reply did not call `send_message`, the worker injects a
/// system nudge and retries up to this many times before parking the request
/// as `NoEgress`.
pub const MAX_PINGPONG_RETRIES: u8 = 2;

/// Hard cap on `get_session` pagination.
///
/// Keeps the cross-session lookup cheap so an agent cannot exhaust memory
/// pulling its sibling's history.
pub const MAX_GET_SESSION_LIMIT: u32 = 200;

/// Maximum bytes for `send_message.context_summary`.
///
/// Honoured only on the first `send_message` to a new receiver inside a DAG;
/// subsequent calls drop the field.
pub const CONTEXT_SUMMARY_MAX_BYTES: usize = 4096;

/// Per-thread broadcast channel buffer for the fan-in DAG stream.
///
/// Sized larger than [`MAX_CHUNK_BUFFER_PER_REQUEST`] because a thread fans in
/// chunks from every request in the DAG; a chatty multi-agent conversation can
/// quickly outpace a single client. A slow SSE subscriber that exhausts this
/// buffer receives a `Stalled` event and must reconnect — same pattern the
/// per-request hub already uses.
pub const MAX_THREAD_CHUNK_BUFFER: usize = 1024;

/// In-process bound on the directory of live thread broadcast slots.
///
/// Same rationale as [`MAX_RESPONSE_SLOTS`](super::pg_response::MAX_RESPONSE_SLOTS):
/// caps memory growth across a long-lived process. Eviction prefers the
/// oldest entry with no live receivers.
pub const MAX_THREAD_SLOTS: usize = 1024;

/// Server-side cap on `GET /threads?limit=`. The default is below this; clients
/// that ask for more are clamped down. Bounded so a single page never serialises
/// an unbounded join through the messages table.
pub const MAX_THREAD_LIST_LIMIT: u32 = 100;

/// Default page size for `GET /threads` when the caller omits `limit=`.
pub const DEFAULT_THREAD_LIST_LIMIT: u32 = 50;

/// Server-side cap on `GET /threads/{id}/messages?limit=`. The default matches.
/// Per [doc/backend_plan.md](../../doc/backend_plan.md): "Page size cap: 1000".
pub const MAX_THREAD_HISTORY_LIMIT: u32 = 1000;

/// Default page size for `GET /threads/{id}/messages`.
///
/// Below the cap so a default fetch never serialises the full JSONB body of
/// every message in a chatty DAG; clients that need more paginate
/// explicitly.
pub const DEFAULT_THREAD_HISTORY_LIMIT: u32 = 100;

/// Characters reserved for the `preview` column on a thread list row.
///
/// Postgres `LEFT(text, n)` is character-bounded (not byte-bounded), so a
/// multibyte prompt yields up to `n × 4` bytes; that's still tiny next to
/// the 64KB prompt cap and matches the Discord-style UI contract.
pub const THREAD_PREVIEW_MAX_CHARS: u32 = 280;

/// Postgres `LISTEN` channel for the fan-in thread-stream subscriber.
///
/// The response hub publishes here on every chunk insert; the fan-in
/// subscriber attaches once per process. Single shared name; the JSON
/// payload carries `(request_id, root_request_id, chunk_seq)`.
pub const THREAD_NOTIFY_CHANNEL: &str = "patom_thread_chunk";
