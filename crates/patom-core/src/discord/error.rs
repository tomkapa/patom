//! `DiscordError` — single module-level error type for the Discord adapter.
//!
//! Every public function in `src/discord/` returns `Result<_, DiscordError>`;
//! `?`-propagation bridges sub-errors via `#[from]` (CLAUDE.md §12).

use thiserror::Error;

use crate::agents::AgentStoreError;
use crate::crypto::CryptoError;
use crate::runtime::PromptError;
use crate::types::ParseError;

use super::types::{ApplicationId, FatalClose};

#[derive(Debug, Error)]
pub enum DiscordError {
    /// A Gateway protocol failure: the `GET /gateway/bot` handshake, the WSS
    /// upgrade, or an unexpected opcode/frame shape.
    #[error("gateway: {0}")]
    Gateway(String),

    /// The Gateway closed with a fatal, non-recoverable code (4004 / 4010-4014).
    /// The connection loop stops and the admin must fix credentials or intents
    /// (4014 is the "agent posts but hears nothing" footgun — a privileged intent
    /// is not enabled in the Developer Portal).
    #[error("gateway closed fatally: {0}")]
    GatewayClosed(FatalClose),

    /// A REST call (`POST /channels/{id}/messages`, the roster/history reads, the
    /// interaction callback) returned a non-OK status after retries.
    #[error("post failed: status={status} body={body}")]
    PostFailed { status: u16, body: String },

    /// Discord told us to back off; the poster handles in-budget retries, so this
    /// only escapes after retries are exhausted.
    #[error("rate-limited; retry after {retry_after_secs}s")]
    RateLimited { retry_after_secs: u32 },

    #[error("post timeout after {0:?}")]
    PostTimeout(std::time::Duration),

    /// An event / REST call referenced an `application_id` we have no
    /// registration for.
    #[error("unknown app: {0}")]
    UnknownApp(ApplicationId),

    /// Downloading an inbound message attachment from the Discord CDN failed
    /// (transport, non-OK status, or the body exceeded the size cap). Non-fatal:
    /// the bridge skips the attachment and mirrors the message without it.
    #[error("attachment fetch: {0}")]
    AttachmentFetch(String),

    /// HTTP transport error talking to Discord.
    #[error("http: {0}")]
    Http(#[from] reqwest::Error),

    /// JSON parse failure on a Discord response/event body.
    #[error("decode: {0}")]
    Decode(#[from] serde_json::Error),

    /// Boundary parse failure (newtype `TryFrom` rejected the value).
    #[error("parse: {0}")]
    Parse(#[from] ParseError),

    #[error("crypto: {0}")]
    Crypto(#[from] CryptoError),

    #[error("db: {0}")]
    Db(#[from] sqlx::Error),

    #[error("queue: {0}")]
    Queue(#[from] PromptError),

    #[error("agents: {0}")]
    Agents(#[from] AgentStoreError),

    /// Catch-all for invariant violations that don't fit a more specific
    /// variant. The message identifies the call site. Used sparingly.
    #[error("internal: {0}")]
    Internal(String),
}
