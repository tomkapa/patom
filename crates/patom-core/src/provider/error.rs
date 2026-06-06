use thiserror::Error;

/// One error type at the provider boundary.
///
/// Callers exhaustively match on this per CLAUDE.md §12. New providers must map their
/// failures into these variants — they may not leak provider-specific error types up.
#[derive(Debug, Error)]
pub enum ProviderError {
    #[error("provider rejected the request: {0}")]
    InvalidRequest(String),

    #[error("provider authentication failed")]
    Unauthorized,

    #[error("provider rate limited the request")]
    RateLimited,

    #[error("provider returned a transient error: {0}")]
    Transient(String),

    #[error("provider returned an empty response")]
    EmptyResponse,

    #[error("provider transport: {0}")]
    Transport(String),

    #[error("provider returned data we could not parse: {0}")]
    Decode(String),
}
