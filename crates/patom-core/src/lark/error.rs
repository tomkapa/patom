//! `LarkError` — single module-level error type for the Lark adapter.
//!
//! Every public function in `src/lark/` returns `Result<_, LarkError>`;
//! `?`-propagation bridges sub-errors via `#[from]` (CLAUDE.md §12).

use thiserror::Error;

use crate::agents::AgentStoreError;
use crate::crypto::CryptoError;
use crate::runtime::PromptError;
use crate::types::ParseError;

use super::pbbp2::Pbbp2Error;
use super::types::LarkAppId;

#[derive(Debug, Error)]
pub enum LarkError {
    /// The endpoint handshake (`/callback/ws/endpoint`) failed or returned a
    /// malformed `ClientConfig`.
    #[error("handshake: {0}")]
    Handshake(String),

    /// `tenant_access_token/internal` mint failed (bad credentials / transport).
    #[error("token mint failed: {0}")]
    TokenMint(String),

    /// The endpoint reported the per-app connection cap (`ExceedConnLimit`).
    #[error("connection limit reached for app {0}")]
    ConnLimit(LarkAppId),

    /// pbbp2 frame encode/decode/reassembly failure.
    #[error("codec: {0}")]
    Codec(#[from] Pbbp2Error),

    /// `im/v1/messages` (or another REST call) returned a non-OK status after
    /// retries.
    #[error("post failed: status={status} body={body}")]
    PostFailed { status: u16, body: String },

    /// Lark told us to back off; the poster handles in-budget retries, so this
    /// only escapes after retries are exhausted.
    #[error("rate-limited; retry after {retry_after_secs}s")]
    RateLimited { retry_after_secs: u32 },

    #[error("post timeout after {0:?}")]
    PostTimeout(std::time::Duration),

    /// A frame / event referenced an `app_id` we have no registration for.
    #[error("unknown app: {0}")]
    UnknownApp(LarkAppId),

    /// HTTP transport error talking to Lark.
    #[error("http: {0}")]
    Http(#[from] reqwest::Error),

    /// JSON parse failure on a Lark response/event body.
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
