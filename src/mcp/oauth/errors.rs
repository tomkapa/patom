use thiserror::Error;

use crate::crypto::CryptoError;
use crate::mcp::McpError;

#[derive(Debug, Error)]
pub enum OAuthError {
    #[error("discovery: {0}")]
    Discovery(String),

    #[error("dynamic client registration: {0}")]
    Dcr(String),

    #[error("token endpoint: {0}")]
    TokenEndpoint(String),

    #[error("invalid callback state")]
    InvalidState,

    #[error("callback expired")]
    Expired,

    #[error("refresh token revoked")]
    RefreshRevoked,

    #[error("crypto: {0}")]
    Crypto(#[from] CryptoError),

    #[error("db: {0}")]
    Db(#[from] sqlx::Error),

    #[error("mcp store: {0}")]
    Mcp(#[from] McpError),

    #[error("misconfigured: {0}")]
    Misconfigured(String),
}
