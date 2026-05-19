//! `SlackError` — single module-level error type for the Slack adapter.
//!
//! Every public function in `src/slack/` returns `Result<_, SlackError>`;
//! `?`-propagation bridges sub-errors via `#[from]` (CLAUDE.md §12).
//!
//! `VerifyError` (signature verification) lives in `verify.rs` as a
//! sub-enum so the verifier can be tested without pulling in the
//! database / queue / agent dependencies.

use thiserror::Error;

use crate::agents::AgentStoreError;
use crate::crypto::CryptoError;
use crate::runtime::PromptError;
use crate::types::ParseError;

use super::types::SlackUserId;
use super::verify::VerifyError;

#[derive(Debug, Error)]
pub enum SlackError {
    /// Inbound HMAC verification failed — handler returns 401.
    #[error("verify: {0}")]
    Verify(#[from] VerifyError),

    /// Slack user has no `slack_identities` row and no fallback applies.
    /// Bridge posts an ephemeral asking the user to link.
    #[error("identity not linked: {0}")]
    IdentityNotLinked(SlackUserId),

    /// Webhook arrived from a workspace we don't have an install for.
    #[error("unknown workspace: team_id={0}")]
    UnknownWorkspace(String),

    /// `@AgentName` parsed out of the mention does not resolve to an
    /// agent within the org. Bridge falls back to the default agent.
    #[error("agent not found in org: {0:?}")]
    AgentNotFound(String),

    /// `chat.postMessage` returned a non-OK status or `{ok:false}` body
    /// after retries.
    #[error("post failed: status={status} body={body}")]
    PostFailed { status: u16, body: String },

    /// Slack told us to back off; the poster handles in-budget retries,
    /// so this variant only escapes after retries are exhausted.
    #[error("rate-limited; retry after {retry_after_secs}s")]
    RateLimited { retry_after_secs: u32 },

    #[error("post timeout after {0:?}")]
    PostTimeout(std::time::Duration),

    /// HTTP transport error talking to Slack.
    #[error("http: {0}")]
    Http(#[from] reqwest::Error),

    /// JSON parse failure on a Slack response body.
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
    /// variant. The message identifies the call site; the cause is the
    /// underlying error if available. Used sparingly.
    #[error("internal: {0}")]
    Internal(String),
}
