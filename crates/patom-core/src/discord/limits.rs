//! Hard limits for the Discord adapter (CLAUDE.md §5).
//!
//! Everything has a limit, enforced in code; each constant is doc-commented with
//! *why this number*, tied to a documented Discord ceiling where one exists.

use std::time::Duration;

// ─────────────────────────────────────────────────────────────────────────
// Gateway transport
// ─────────────────────────────────────────────────────────────────────────

/// Gateway API version + JSON encoding query (`wss://…/?v=10&encoding=json`).
/// Pinned: a version bump is a deliberate, tested change.
pub const DISCORD_GATEWAY_QUERY: &str = "v=10&encoding=json";

/// Per-attempt timeout for `GET /gateway/bot` (the connect-info fetch).
pub const DISCORD_GATEWAY_BOT_TIMEOUT: Duration = Duration::from_secs(10);

/// Timeout for dialing the negotiated `wss://…` Gateway endpoint.
///
/// Covers TCP + TLS + WS upgrade. Without it a slow/unresponsive edge would hang
/// the connection task indefinitely instead of failing into the reconnect loop.
pub const DISCORD_WS_CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

/// Budget to receive `HELLO` + complete the `IDENTIFY`/`RESUME` handshake after
/// the socket opens, before we give up and reconnect.
pub const DISCORD_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(20);

/// Bound on the inbound Gateway → bridge mpsc. A full queue drops the event
/// (with a counter increment); the `thread_messages.idempotency_key` dedup makes
/// any later redelivery safe.
pub const DISCORD_INBOUND_QUEUE: usize = 256;

/// Finite reconnect budget per bot (§5). A permanently-broken app stops retrying
/// instead of spinning forever; a *fatal* close (4004/4013/4014) stops
/// immediately regardless.
pub const DISCORD_RECONNECT_MAX: u32 = 64;

/// Base reconnect backoff; grows exponentially up to [`DISCORD_RECONNECT_CAP`].
pub const DISCORD_RECONNECT_BASE: Duration = Duration::from_secs(1);

/// Ceiling on a single reconnect backoff wait.
pub const DISCORD_RECONNECT_CAP: Duration = Duration::from_mins(1);

/// Documented Gateway session limits (encode them so we never trip a fatal cap):
///
/// 1000 `IDENTIFY`s per 24h across shards — duplicate connects (a second replica
/// also identifying) burn this budget, which is why we hold a single owner per
/// bot via a Postgres advisory lock.
pub const DISCORD_IDENTIFY_PER_DAY: u32 = 1000;

/// 120 gateway commands per 60s per connection (heartbeats included). The
/// heartbeat + occasional RESUME stay well under this.
pub const DISCORD_GATEWAY_COMMANDS_PER_60S: u32 = 120;

/// Control payloads (IDENTIFY/RESUME/HEARTBEAT) must be < 4096 bytes.
pub const DISCORD_CONTROL_PAYLOAD_MAX_BYTES: usize = 4096;

/// Bound on the hot-connect queue (admin registers a bot → the gateway manager
/// opens its connection without a restart). Overflow drops the request and the
/// bot connects on the next restart.
pub const DISCORD_CONNECT_QUEUE: usize = 64;

// ─────────────────────────────────────────────────────────────────────────
// Outbound REST (poster) + rate limiting
// ─────────────────────────────────────────────────────────────────────────

/// Outbound `POST /channels/{id}/messages` per-attempt timeout.
pub const DISCORD_POST_TIMEOUT: Duration = Duration::from_secs(10);

/// Retry budget for an outbound REST call on 429 / 5xx. Exhaustion drops the
/// chunk and increments a counter.
pub const DISCORD_POST_MAX_RETRIES: u8 = 3;

/// Cap on the `Retry-After` / `retry_after` value the poster will honour,
/// protecting the pump from a misbehaving upstream stalling it indefinitely.
pub const DISCORD_RETRY_AFTER_CAP_SECS: u32 = 30;

/// Hard cap on a single Discord message's content. Discord rejects > 2000
/// characters; the poster chunks output to this boundary.
pub const DISCORD_MESSAGE_MAX: usize = 2000;

/// Global REST limit: ~50 requests/second per bot token. The token-bucket rate
/// limiter is sized from this at startup.
pub const DISCORD_GLOBAL_RATE_PER_SEC: u32 = 50;

/// Cap on distinct outbound mention handles resolved per reply (the addressed
/// colleague → `<@snowflake>` map). Bounds the per-post resolve (§5).
pub const DISCORD_TAG_HANDLES_MAX: usize = 2_000;

/// The Cloudflare invalid-request ban threshold.
///
/// More than 10,000 `401/403/429` responses in a 10-minute window bans the
/// **egress IP** — which aggregates across every tenant bot sharing it, so one
/// noisy tenant can take down the rest. We watch a per-IP gauge against this.
pub const DISCORD_INVALID_REQUEST_BUDGET: u32 = 10_000;

/// The window the invalid-request budget is measured over.
pub const DISCORD_INVALID_REQUEST_WINDOW: Duration = Duration::from_mins(10);

// ─────────────────────────────────────────────────────────────────────────
// Stream pump
// ─────────────────────────────────────────────────────────────────────────

/// Maximum number of concurrent per-thread stream pumps. New attaches beyond
/// this cap evict the oldest idle pump (FIFO by last activity).
pub const MAX_DISCORD_STREAM_PUMPS: usize = 256;

/// Idle TTL for a per-thread pump. If no chunks arrive in this window the pump
/// exits; a future event in the same Discord thread re-attaches fresh.
pub const DISCORD_PUMP_IDLE_TTL: Duration = Duration::from_mins(30);

// ─────────────────────────────────────────────────────────────────────────
// Roster (GUILD_MEMBERS) sync
// ─────────────────────────────────────────────────────────────────────────

/// Page size for `GET /guilds/{id}/members` (its documented ceiling is 1000).
pub const DISCORD_ROSTER_PAGE_SIZE: usize = 1_000;

/// Hard cap on members materialized from one roster sync (bounds the paging
/// loop, §5).
pub const DISCORD_ROSTER_MAX_MEMBERS: usize = 5_000;

/// Hard cap on roster pages walked in one sync (`MAX_MEMBERS / PAGE_SIZE`,
/// rounded up, with head-room).
pub const DISCORD_ROSTER_MAX_PAGES: usize = 8;

// ─────────────────────────────────────────────────────────────────────────
// History backfill
// ─────────────────────────────────────────────────────────────────────────

/// Page size for `GET /channels/{id}/messages` (its documented ceiling is 100).
pub const DISCORD_BACKFILL_PAGE_SIZE: usize = 100;

/// Hard cap on messages mirrored in one channel's first-access backfill (bounds
/// the paging loop, §5). Older history stays unread until a Phase-2 deep sweep.
pub const DISCORD_BACKFILL_MAX_MESSAGES: usize = 1_000;

/// Hard cap on backfill pages walked for one channel (`MAX / PAGE_SIZE`, with
/// head-room).
pub const DISCORD_BACKFILL_MAX_PAGES: usize = 16;

// ─────────────────────────────────────────────────────────────────────────
// Interactions + consent
// ─────────────────────────────────────────────────────────────────────────

/// Deadline to respond to an `INTERACTION_CREATE` — Discord drops the
/// interaction if no callback arrives within 3s (defer with a type-5 ack if the
/// real work is slower).
pub const DISCORD_INTERACTION_DEADLINE: Duration = Duration::from_secs(3);

/// How long an interaction token stays valid for follow-up callbacks.
pub const DISCORD_INTERACTION_TOKEN_TTL: Duration = Duration::from_mins(15);

/// TTL for a shadow→real account link token embedded in the consent button URL
/// (the Slack #41 pattern). Short, so a leaked link expires quickly.
pub const DISCORD_LINK_TOKEN_TTL: Duration = Duration::from_mins(10);
