//! Asset-storage error type. CLAUDE.md §12: one error per module boundary.

use thiserror::Error;

use crate::types::ParseError;

/// All failures that the asset upload/storage layer can surface. The HTTP
/// boundary maps these onto status codes via `From<AssetError> for HttpError`.
#[derive(Debug, Error)]
pub enum AssetError {
    /// The multipart body exceeded the per-kind byte cap.
    #[error("upload too large: max {max} bytes, got at least {got}")]
    TooLarge { max: usize, got: usize },

    /// `Content-Type` header was outside the allow-list for this kind.
    #[error("content type not allowed: {0}")]
    ContentTypeNotAllowed(String),

    /// The first few bytes of the payload don't match the claimed
    /// `Content-Type`. Defends against `evil.svg` claiming `image/png`.
    #[error("content type / magic byte mismatch (claimed {claimed})")]
    MagicByteMismatch { claimed: &'static str },

    /// The body could not be sniffed at all (too short, no signature).
    #[error("unable to identify file type from payload")]
    UnknownFileType,

    /// The multipart envelope contained no `file` part, or contained the
    /// wrong field name.
    #[error("missing multipart field 'file'")]
    MissingField,

    /// The `file` part carried no `filename` in its Content-Disposition.
    /// Required for message attachments so the model and UI can label it.
    #[error("missing attachment filename")]
    MissingFilename,

    /// The multipart envelope contained more than one part.
    #[error("multipart body must contain exactly one field")]
    TooManyFields,

    /// Failed to construct or persist an `ObjectKey` / `AssetUrl`.
    #[error(transparent)]
    Parse(#[from] ParseError),

    /// Underlying multipart decoder failure (malformed boundary, etc.).
    #[error("multipart decode failed: {0}")]
    Multipart(String),

    /// S3 PutObject round-trip failed. Body is the SDK's display string;
    /// the full `DisplayErrorContext` is logged via the §2 5xx tracing
    /// handler.
    #[error("storage put object failed: {0}")]
    StoragePut(String),

    /// S3 DeleteObject round-trip failed.
    #[error("storage delete object failed: {0}")]
    StorageDelete(String),

    /// S3 GetObject round-trip failed (not a missing key — see
    /// [`Self::NotFound`] for that).
    #[error("storage get object failed: {0}")]
    StorageGet(String),

    /// The requested object key does not exist in storage. Distinct from
    /// [`Self::StorageGet`] so a fetch-and-inject of a stale asset surfaces as a
    /// clean "not found" rather than an opaque backend error.
    #[error("object not found")]
    NotFound,

    /// Per-call timeout fired before object storage responded. Distinct
    /// variant so retries and dashboards can separate "slow storage" from
    /// "broken storage".
    #[error("storage operation timed out")]
    Timeout,
}
