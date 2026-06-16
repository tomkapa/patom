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

    /// A message carried attachment content of a kind the target model does
    /// not accept (issue #187). Raised by the converter *before* dispatch so a
    /// text-only backend on the shared OpenAI wire path (DeepSeek) never
    /// receives an image/file part. `mime` is the rejected content type;
    /// `model` is the catalog name it was routed to.
    #[error("model `{model}` does not accept `{mime}` input")]
    UnsupportedContent { mime: &'static str, model: String },

    /// Failed to fetch or transform an attachment's bytes before dispatch
    /// (download error, oversize body, or Office text-extraction failure).
    #[error("attachment processing failed: {0}")]
    Attachment(String),
}
