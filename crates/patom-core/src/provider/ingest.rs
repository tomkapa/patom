//! Shared "raw bytes → stored [`Attachment`]" ingest path, plus a bounded
//! HTTP fetch used to pull those bytes from an external platform.
//!
//! Three call sites need to turn bytes they already hold into a message
//! attachment: the chat-UI upload route (`POST /uploads/attachment`), the
//! Discord bridge (CDN download), and the Lark bridge (resource download). They
//! all want the same thing — store the bytes under a fresh, immutable
//! `attachments/{uuid}.{ext}` key and hand back a validated [`Attachment`] whose
//! URL is rooted at the asset-store origin (so `/prompts`' SSRF guard and the
//! provider fetchers accept it). [`ingest_attachment`] is the one place that
//! logic lives, so the bridges and the route can never drift.
//!
//! [`get_capped`] is the matching read side: a size- and time-bounded GET that
//! the platform fetchers wrap with their own auth (CLAUDE.md §5 — every I/O
//! await is bounded in time *and* size).

use bytes::Bytes;
use thiserror::Error;
use uuid::Uuid;

use crate::assets::{
    AssetContentType, AssetError, ObjectKey, SharedAssetStore, validate_attachment_bytes,
};
use crate::types::ParseError;

use super::attachment::{Attachment, FileName, RawAttachment};
use super::limits::ATTACHMENT_FETCH_TIMEOUT;

/// Failure turning held bytes into a stored [`Attachment`].
#[derive(Debug, Error)]
pub enum IngestError {
    /// A newtype `TryFrom` rejected the value (bad filename, malformed url, …).
    #[error("parse: {0}")]
    Parse(#[from] ParseError),
    /// The trust-boundary byte check or the object-store write failed.
    #[error("store: {0}")]
    Store(#[from] AssetError),
}

/// Store `bytes` as a message attachment and return the validated reference.
///
/// `content_type` is the storage content-type — already narrowed to the
/// attachment allow-list by the caller (no SVG: it is not valid model input).
/// `filename` is the display name shown to the model. The bytes are re-checked
/// against the same size + magic-byte trust boundary the HTTP upload path
/// applies ([`validate_attachment_bytes`]), so *every* caller — route or bridge
/// — stores only size-bounded, content-verified objects and a mislabeled
/// payload can never reach object storage.
///
/// The object lands under an immutable `attachments/{uuid}.{ext}` key, mirroring
/// `POST /uploads/attachment` exactly.
pub async fn ingest_attachment(
    store: &SharedAssetStore,
    filename: &str,
    content_type: AssetContentType,
    bytes: Bytes,
) -> Result<Attachment, IngestError> {
    // Trust-boundary re-validation (size cap + empty + magic-byte cross-check).
    // Runs before any store write, so an oversize/mislabeled/empty payload fails
    // fast and never leaves an orphan object — and never trips the non-empty-body
    // assertion in the store.
    validate_attachment_bytes(&bytes, content_type, content_type.attachment_max_bytes())?;
    // Validate the display name before the write too (same reason).
    FileName::try_from(filename)?;
    let size = u64::try_from(bytes.len()).unwrap_or(u64::MAX);

    let key_raw = format!(
        "attachments/{}.{}",
        Uuid::new_v4(),
        content_type.extension()
    );
    let key = ObjectKey::try_from(key_raw.as_str())?;
    let url = store.put(key, bytes, content_type).await?;

    // The single deserialization funnel (CLAUDE.md §1): re-parse everything,
    // including the freshly-minted URL, through `Attachment::try_from`.
    let attachment = Attachment::try_from(RawAttachment {
        url: url.as_str().to_owned(),
        mime: content_type.as_mime().to_owned(),
        filename: filename.to_owned(),
        size,
    })?;
    Ok(attachment)
}

/// Bytes pulled from an external URL, with the response's advertised
/// `Content-Type` (the most reliable type signal for a Lark image, which the
/// inbound event does not otherwise label).
#[derive(Debug, Clone)]
pub struct FetchedBytes {
    pub bytes: Bytes,
    pub content_type: Option<String>,
}

/// Failure fetching attachment bytes from an external platform.
#[derive(Debug, Error)]
pub enum FetchError {
    #[error("http status {status}")]
    Status { status: u16 },
    #[error("transport: {0}")]
    Transport(String),
    #[error("attachment too large: max {max}, got {got}")]
    TooLarge { max: u64, got: u64 },
    #[error("timed out")]
    Timeout,
}

/// GET the bytes behind `req`, bounded in time and size.
///
/// Bounded by [`ATTACHMENT_FETCH_TIMEOUT`] and `max`. `req` carries the URL and
/// any auth the caller attached, so this is shared by the unauthenticated
/// Discord CDN fetch and the bearer-authenticated Lark resource fetch alike;
/// each maps [`FetchError`] into its own module error.
pub async fn get_capped(
    req: reqwest::RequestBuilder,
    max: u64,
) -> Result<FetchedBytes, FetchError> {
    let fetch = async {
        let resp = req
            .send()
            .await
            .map_err(|e| FetchError::Transport(e.to_string()))?;
        let status = resp.status();
        if !status.is_success() {
            return Err(FetchError::Status {
                status: status.as_u16(),
            });
        }
        // Reject on the advertised length before downloading the body.
        if let Some(len) = resp.content_length()
            && len > max
        {
            return Err(FetchError::TooLarge { max, got: len });
        }
        let content_type = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(ToOwned::to_owned);
        let bytes = resp
            .bytes()
            .await
            .map_err(|e| FetchError::Transport(e.to_string()))?;
        // A lying / absent Content-Length is still caught after the read.
        let got = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
        if got > max {
            return Err(FetchError::TooLarge { max, got });
        }
        Ok(FetchedBytes {
            bytes,
            content_type,
        })
    };
    tokio::time::timeout(ATTACHMENT_FETCH_TIMEOUT, fetch)
        .await
        .map_err(|_| FetchError::Timeout)?
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::assets::InMemoryAssetStore;
    use crate::provider::AttachmentMime;

    const PNG_HEADER: &[u8] = &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];

    fn png(total: usize) -> Bytes {
        let mut v = PNG_HEADER.to_vec();
        v.resize(total, 0);
        Bytes::from(v)
    }

    /// A concrete fake (for `.is_empty()` inspection) plus the same value typed
    /// as the trait object `ingest_attachment` consumes.
    fn store() -> (Arc<InMemoryAssetStore>, SharedAssetStore) {
        let concrete = Arc::new(InMemoryAssetStore::new("https://asset.example"));
        let shared: SharedAssetStore = concrete.clone();
        (concrete, shared)
    }

    #[tokio::test]
    async fn ingests_png_into_attachment_under_asset_origin() {
        let (fake, shared) = store();
        let att = ingest_attachment(&shared, "screenshot.png", AssetContentType::Png, png(64))
            .await
            .expect("ingest ok");
        assert!(
            att.url()
                .as_str()
                .starts_with("https://asset.example/attachments/")
        );
        assert!(att.url().as_str().ends_with(".png"));
        assert_eq!(att.mime(), AttachmentMime::Png);
        assert_eq!(att.filename().as_str(), "screenshot.png");
        assert_eq!(att.size(), 64);
        assert_eq!(fake.len().await, 1, "exactly one object stored");
    }

    #[tokio::test]
    async fn rejects_oversize_before_storing() {
        let (fake, shared) = store();
        let over = usize::try_from(AttachmentMime::Png.max_bytes()).expect("fits") + 1;
        let err = ingest_attachment(&shared, "big.png", AssetContentType::Png, png(over))
            .await
            .expect_err("rejected");
        assert!(matches!(
            err,
            IngestError::Store(AssetError::TooLarge { .. })
        ));
        assert!(fake.is_empty().await, "no orphan object on rejection");
    }

    #[tokio::test]
    async fn rejects_mislabeled_bytes() {
        // PNG bytes labeled as a PDF must not be stored (magic-byte mismatch).
        let (fake, shared) = store();
        let err = ingest_attachment(&shared, "evil.pdf", AssetContentType::Pdf, png(64))
            .await
            .expect_err("rejected");
        assert!(matches!(
            err,
            IngestError::Store(AssetError::MagicByteMismatch { .. })
        ));
        assert!(fake.is_empty().await);
    }

    #[tokio::test]
    async fn rejects_bad_filename() {
        let (fake, shared) = store();
        let err = ingest_attachment(&shared, "../etc/passwd", AssetContentType::Png, png(64))
            .await
            .expect_err("rejected");
        assert!(matches!(
            err,
            IngestError::Parse(ParseError::Malformed { .. })
        ));
        assert!(fake.is_empty().await);
    }
}
