//! Lark adapter bounds (CLAUDE.md §5 — everything has a limit).
//!
//! Notes on the numbers below:
//!
//! - The WS ACK budget is below Lark's hard 3-second response deadline with a
//!   half-second margin for frame decode + `try_send`.
//! - The per-app connection cap mirrors Lark's documented `ExceedConnLimit`
//!   ceiling of 50 long-connections per app.
//! - History `page_size` caps at 50 (the list-messages API ceiling); the roster
//!   members API ceiling is 100. (History sync is the deferred backfill; the
//!   roster cap applies here.)

use std::time::Duration;

/// Budget to ACK a data frame (Lark's deadline is ~3 s).
///
/// We keep half a second in reserve for frame decode + `mpsc::try_send`, and
/// ACK before dispatching the agent run, so this budget is never on the slow
/// path.
pub const LARK_WS_ACK_BUDGET: Duration = Duration::from_millis(2_500);

/// Bound on the inbound WS → bridge mpsc. A full queue drops the event (with a
/// counter increment); Lark re-delivers on a missed ACK, and the
/// `thread_messages.idempotency_key` dedup makes redelivery safe.
pub const LARK_INBOUND_QUEUE: usize = 256;

/// Maximum long-connections per Lark app (Lark's documented ceiling).
///
/// Lark rejects the 51st with `ExceedConnLimit`; we keep a single owner per bot
/// so we never approach it, but assert against it on connect.
pub const LARK_MAX_CONNS_PER_APP: usize = 50;

/// Maximum partial (fragmented) messages buffered in the reassembler at once.
///
/// A large event is split into `sum` frames sharing a `message_id`; this caps
/// how many distinct in-flight messages we hold before evicting the oldest.
pub const LARK_FRAME_REASSEMBLY_MAX: usize = 64;

/// Maximum number of fragments a single message may declare (`sum`). Guards the
/// per-message fragment buffer and the concatenated-payload size.
pub const LARK_FRAME_MAX_FRAGMENTS: u32 = 256;

/// Finite reconnect budget per bot. Lark's `ClientConfig.ReconnectCount`
/// default is unbounded (`-1`); we cap it (§5) so a permanently-broken app
/// stops retrying instead of spinning forever.
pub const LARK_RECONNECT_MAX: u32 = 64;

/// Fallback ping interval if the handshake `ClientConfig` omits one. Lark's
/// default is ~120 s.
pub const LARK_DEFAULT_PING_INTERVAL: Duration = Duration::from_mins(2);

/// Fallback reconnect backoff if `ClientConfig.ReconnectInterval` is absent.
pub const LARK_DEFAULT_RECONNECT_INTERVAL: Duration = Duration::from_secs(2);

/// Per-attempt timeout for the endpoint handshake POST.
pub const LARK_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// Per-attempt timeout for the `tenant_access_token/internal` mint.
pub const LARK_TOKEN_TIMEOUT: Duration = Duration::from_secs(10);

/// Re-mint a `tenant_access_token` this long before its reported `expire`, so a
/// request never races the expiry boundary.
pub const LARK_TOKEN_REFRESH_SKEW: Duration = Duration::from_mins(5);

/// Outbound `im/v1/messages` per-attempt timeout. Below the default `reqwest`
/// timeout so a hung Lark edge does not stall the pump.
pub const LARK_POST_TIMEOUT: Duration = Duration::from_secs(10);

/// Retry budget for `im/v1/messages` on 429 / 5xx. Exhaustion drops the chunk
/// and increments a counter.
pub const LARK_POST_MAX_RETRIES: u8 = 3;

/// Cap on the `Retry-After` value the poster will honour, protecting the pump
/// from a misbehaving upstream stalling it indefinitely.
pub const LARK_RETRY_AFTER_CAP_SECS: u32 = 30;

/// Cap on the agent-reply text length forwarded to Lark. Lark text messages
/// accept up to 30 000 bytes; clip lower so `<at>` markup overhead never tips
/// us over.
pub const LARK_MAX_POST_CHARS: usize = 25_000;

/// Page size for the chat-members roster API (its documented ceiling is 100).
pub const LARK_ROSTER_PAGE_SIZE: usize = 100;

/// Hard cap on members materialized from one roster sync (bounds the paging
/// loop, §5).
pub const LARK_ROSTER_MAX_MEMBERS: usize = 5_000;

/// Hard cap on roster pages walked in one sync (`MAX_MEMBERS / PAGE_SIZE`,
/// rounded up, with head-room).
pub const LARK_ROSTER_MAX_PAGES: usize = 64;

/// Bound on the hot-connect queue (admin registers a bot → the WS manager opens
/// its connection without a restart).
///
/// A full queue is implausible (registrations are rare, admin-driven); overflow
/// just drops the request and the bot connects on the next restart.
pub const LARK_CONNECT_QUEUE: usize = 64;

/// Cap on `@`-taggable Lark handles fetched per outbound reply.
///
/// The org's Lark humans, used to rewrite `@Name` → `<at>` markup. Bounds the
/// per-post name→open_id map (§5).
pub const LARK_TAG_HANDLES_MAX: usize = 2_000;

/// Maximum number of concurrent per-thread stream pumps. New attaches beyond
/// this cap evict the oldest idle pump (FIFO by last activity).
pub const MAX_LARK_STREAM_PUMPS: usize = 256;

/// Idle TTL for a per-thread pump. If no chunks arrive in this window the pump
/// exits; a future event in the same Lark thread re-attaches fresh.
pub const LARK_PUMP_IDLE_TTL: Duration = Duration::from_mins(30);
