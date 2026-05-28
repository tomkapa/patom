use thiserror::Error;

use crate::crypto::CryptoError;
use crate::mcp::McpError;

/// Errors crossing the OAuth subsystem boundary.
///
/// Most operational failures (discovery, DCR, code exchange, refresh) now
/// originate inside `rmcp::transport::auth` and surface here via the
/// `Rmcp` variant — patom's own variants cover patom-owned seams
/// (configuration, encryption, DB).
#[derive(Debug, Error)]
pub enum OAuthError {
    /// Surfaces an error from `rmcp::transport::auth::*`. Wraps rmcp's
    /// `AuthError` directly so call sites can `?`-propagate without
    /// stringifying intermediate failure modes (mismatched issuers,
    /// failed DCR, revoked refresh tokens — rmcp's enum names them
    /// explicitly and tooling can pattern-match).
    #[error("rmcp auth: {0}")]
    Rmcp(#[from] rmcp::transport::auth::AuthError),

    #[error("crypto: {0}")]
    Crypto(#[from] CryptoError),

    #[error("db: {0}")]
    Db(#[from] sqlx::Error),

    #[error("mcp store: {0}")]
    Mcp(#[from] McpError),

    /// Catalog or env-driven configuration is inconsistent (e.g.
    /// `client_source = 'platform'` but the env vars are missing, or a
    /// callback that arrives without a pending row).
    #[error("misconfigured: {0}")]
    Misconfigured(String),
}
