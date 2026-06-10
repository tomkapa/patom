//! Slack adapter bounds (CLAUDE.md §5 — everything has a limit).
//!
//! Notes on the numbers below:
//!
//! - The webhook ack budget is below Slack's hard 3-second timeout with a
//!   half-second margin for the inbound TLS handshake and `try_send` cost.
//! - The inbound queue size is the same order as the existing prompt queue
//!   so a worker stall surfaces as drops rather than memory growth.
//! - Body cap 96 KB is ~1.5× the largest `event_callback` envelope Slack
//!   documents in the wild.
//! - Timestamp skew of 5 minutes is Slack's recommended threshold.
//! - Per-pump idle TTL caps how long a pump holds a `broadcast::Receiver`
//!   after a DAG quiesces.

use std::time::Duration;

/// Slack waits at most 3 s for a `200`; we keep half a second in reserve
/// for the TLS handshake, header parse, and `mpsc::try_send`.
pub const SLACK_ACK_BUDGET: Duration = Duration::from_millis(2_500);

/// Bound on the inbound webhook → bridge mpsc. A full queue drops the
/// event (with a counter increment); Slack will retry up to 3× with
/// exponential backoff, so transient bridge stalls are recoverable.
pub const SLACK_INBOUND_QUEUE: usize = 256;

/// Hard cap on webhook body bytes. Slack `event_callback` envelopes top
/// out near 64 KB in practice; this leaves head-room without inviting
/// abuse.
pub const SLACK_WEBHOOK_MAX_BYTES: usize = 96 * 1024;

/// Maximum tolerated absolute skew between `now()` and the value in the
/// `X-Slack-Request-Timestamp` header.
///
/// Matches Slack's recommended threshold and is the gate against replay
/// attacks — the signature alone is forgeable from a captured payload if
/// the timestamp is not validated.
pub const SLACK_TIMESTAMP_MAX_SKEW: Duration = Duration::from_mins(5);

/// Outbound `chat.postMessage` per-attempt timeout. Below the default
/// `reqwest` timeout so a hung Slack edge does not stall the pump.
pub const SLACK_POST_TIMEOUT: Duration = Duration::from_secs(10);

/// Per-attempt timeout for `users.info` lookups.
///
/// Runs inside the Slack 3 s `view_submission` ack window (slash
/// command path). Tighter than `SLACK_POST_TIMEOUT` because the
/// lookup is best-effort enrichment — the caller falls back to the
/// slash form `user_name` + default avatar on any failure, and
/// missing the 3 s window costs the user a generic Slack error UI.
pub const SLACK_USERS_INFO_TIMEOUT: Duration = Duration::from_millis(1_500);

/// Maximum number of concurrent per-thread stream pumps. New attaches
/// beyond this cap evict the oldest idle pump (FIFO by last activity).
pub const MAX_SLACK_STREAM_PUMPS: usize = 256;

/// Idle TTL for a per-thread pump. If no chunks arrive in this window
/// the pump exits; a future event in the same Slack thread re-attaches
/// fresh via `bridge::process_event`.
pub const SLACK_PUMP_IDLE_TTL: Duration = Duration::from_mins(30);

/// Retry budget for `chat.postMessage` on 429 / 5xx / `error: ratelimited`.
/// Exhaustion drops the chunk and increments a counter; durable retries
/// are GitHub issue #44 (post outbox).
pub const SLACK_POST_MAX_RETRIES: u8 = 3;

/// Cap on the `Retry-After` value the poster will honour. Slack-issued
/// values are typically <30s; clamping protects us from a misbehaving
/// upstream or a hostile MITM stalling the pump task indefinitely.
pub const SLACK_RETRY_AFTER_CAP_SECS: u32 = 30;

/// Per-attempt timeout for reading the response body from Slack.
///
/// Separate from the request-level `SLACK_POST_TIMEOUT` because
/// reqwest's response stream is its own I/O surface (CLAUDE.md §5 —
/// every await against I/O is timed).
pub const SLACK_POST_BODY_TIMEOUT: Duration = Duration::from_secs(5);

/// Cap on the agent-reply text length we forward to Slack. Slack's
/// `chat.postMessage` accepts up to 40 000 characters, but we clip a bit
/// lower so block/markdown overhead never tips us over.
pub const SLACK_MAX_POST_CHARS: usize = 35_000;

/// Maximum number of agents rendered in the `/patom` Block Kit picker.
///
/// Slack's `static_select` element hard-caps options at 100; tenants
/// with more agents see the first 100 alphabetically and fall back to
/// the `@PatomBot <agent-name>` mention path for the long tail (the
/// modal copy points this out).
pub const MAX_AGENTS_IN_PICKER: usize = 100;

/// Maximum characters for the prompt text area in the `/patom` modal.
///
/// Slack's `plain_text_input.max_length` accepts up to 3 000; the
/// downstream `Prompt` newtype caps at 64 KB, so the modal is the
/// tighter gate. Sized at Slack's ceiling so users get the full
/// allowance the surface permits.
pub const SLACK_SLASH_PROMPT_MAX_CHARS: u32 = 3_000;

/// Maximum characters for the recruiter's `reason` paragraph.
///
/// Rendered inside an MCP connection-request card's section block.
/// Slack's `section.text.mrkdwn` accepts up to 3 000; the tool-side
/// schema caps reason at 512 bytes, so we have head-room — but we cap
/// here too so a future tool-side bump can't surprise the renderer.
pub const SLACK_CONNECTION_REASON_MAX_CHARS: usize = 2_000;

/// Maximum bytes for a Slack modal's `private_metadata` field.
///
/// Slack's hard limit is 3 000 bytes; we keep ~1 KB of head-room for
/// forward compatibility (extra keys we may add to the routing
/// payload). The field is plaintext and visible to Slack, so it
/// carries only `team_id` / `channel_id` / `user_id` — never secrets.
pub const MAX_PRIVATE_METADATA_BYTES: usize = 2_000;
