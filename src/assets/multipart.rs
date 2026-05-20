//! HTTP multipart trust boundary for the asset module.
//!
//! Every byte coming in here is untrusted. We enforce three invariants
//! before anything is handed to [`crate::assets::AssetStore`]: size cap,
//! content-type allow-list, and magic-byte cross-check.
//!
//!   1. **Size** — the body cannot exceed the per-kind byte cap.
//!      CLAUDE.md §5: every external input is length-capped.
//!   2. **Content type** — the claimed `Content-Type` must be inside the
//!      [`AssetKind`]-specific allow-list.
//!   3. **Magic byte** — the payload's first bytes must match the
//!      claimed type. Defends against an `evil.svg` mis-claiming
//!      `image/png` to dodge the SVG-XSS denial on the avatar path.

use std::future::Future;

use axum::extract::Multipart;
use bytes::Bytes;
use tokio::time::timeout;

use super::error::AssetError;
use super::limits::{MULTIPART_IO_TIMEOUT, SNIFF_PREFIX_BYTES};
use super::traits::{AssetKind, ImageContentType};

/// Wrap a multipart I/O await in `MULTIPART_IO_TIMEOUT`. A slow client
/// trickling bytes can't hold the request task indefinitely.
async fn with_timeout<F, T, E>(fut: F) -> Result<T, AssetError>
where
    F: Future<Output = Result<T, E>>,
    E: std::fmt::Display,
{
    match timeout(MULTIPART_IO_TIMEOUT, fut).await {
        Err(_) => Err(AssetError::Timeout),
        Ok(Ok(v)) => Ok(v),
        Ok(Err(e)) => Err(AssetError::Multipart(e.to_string())),
    }
}

/// Single image part extracted from a multipart body. Returned by
/// [`extract_single_image_field`].
#[derive(Debug)]
pub struct UploadedImage {
    pub bytes: Bytes,
    pub content_type: ImageContentType,
}

/// Read one image from a multipart body.
///
/// Drains `multipart` expecting exactly one field named `file`, parses
/// and validates it for `kind`, and returns the bytes. The per-kind
/// byte cap is sourced from [`AssetKind::max_bytes`]. The first error
/// short-circuits — we never bill bytes against R2 on a bad payload.
pub async fn extract_single_image_field(
    mut multipart: Multipart,
    kind: AssetKind,
) -> Result<UploadedImage, AssetError> {
    let field = with_timeout(multipart.next_field())
        .await?
        .ok_or(AssetError::MissingField)?;

    if field.name() != Some("file") {
        return Err(AssetError::MissingField);
    }
    let claimed = field
        .content_type()
        .and_then(ImageContentType::from_mime)
        .ok_or_else(|| {
            AssetError::ContentTypeNotAllowed(
                field.content_type().unwrap_or("<missing>").to_owned(),
            )
        })?;
    if !kind.accepts(claimed) {
        return Err(AssetError::ContentTypeNotAllowed(claimed.as_mime().into()));
    }

    let bytes = with_timeout(field.bytes()).await?;

    // After reading the first field, the multipart stream MUST end. A
    // second field is a misshaped request — refuse before we touch R2.
    if with_timeout(multipart.next_field()).await?.is_some() {
        return Err(AssetError::TooManyFields);
    }

    validate_image_bytes(&bytes, claimed, kind.max_bytes())?;

    Ok(UploadedImage {
        bytes,
        content_type: claimed,
    })
}

/// Pure byte-level validation: size + magic-byte cross-check.
///
/// Split out from the multipart loop so unit tests can drive it without
/// standing up an `axum::Multipart`. CLAUDE.md §3 — the trust-boundary
/// helper has its own table-driven tests.
pub fn validate_image_bytes(
    bytes: &[u8],
    claimed: ImageContentType,
    max_bytes: usize,
) -> Result<(), AssetError> {
    if bytes.len() > max_bytes {
        return Err(AssetError::TooLarge {
            max: max_bytes,
            got: bytes.len(),
        });
    }
    if bytes.is_empty() {
        return Err(AssetError::UnknownFileType);
    }
    let prefix = &bytes[..bytes.len().min(SNIFF_PREFIX_BYTES)];
    let sniffed = sniff_image_type(prefix).ok_or(AssetError::UnknownFileType)?;
    if sniffed != claimed {
        return Err(AssetError::MagicByteMismatch {
            claimed: claimed.as_mime(),
        });
    }
    Ok(())
}

/// Detect which [`ImageContentType`] a byte prefix matches, or `None`
/// if it's not any of the allow-listed types. Uses `infer` for the
/// binary formats (PNG / JPEG / WebP) and a small in-line XML sniff for
/// SVG since `infer`'s SVG detection has had inconsistent coverage
/// across versions.
fn sniff_image_type(prefix: &[u8]) -> Option<ImageContentType> {
    if let Some(t) = infer::get(prefix) {
        match t.mime_type() {
            "image/png" => return Some(ImageContentType::Png),
            "image/jpeg" => return Some(ImageContentType::Jpeg),
            "image/webp" => return Some(ImageContentType::Webp),
            "image/svg+xml" => return Some(ImageContentType::Svg),
            // Some infer versions tag XML as "text/xml" for an SVG
            // payload that begins with `<?xml ...?>`. Fall through to
            // our prefix check instead of failing closed.
            _ => {}
        }
    }
    // Fallback: SVG isn't a magic-byte format — match the textual
    // prefix. Skip BOM + leading whitespace, then check for `<?xml`
    // or `<svg`. Defends against trailing junk after the SVG root by
    // only inspecting the prefix.
    let trimmed = trim_leading_xml(prefix);
    if trimmed.starts_with(b"<?xml") || trimmed.starts_with(b"<svg") {
        return Some(ImageContentType::Svg);
    }
    None
}

fn trim_leading_xml(prefix: &[u8]) -> &[u8] {
    let mut i = 0;
    // Strip a UTF-8 BOM if present (EF BB BF).
    if prefix.starts_with(&[0xEF, 0xBB, 0xBF]) {
        i += 3;
    }
    while i < prefix.len() && prefix[i].is_ascii_whitespace() {
        i += 1;
    }
    &prefix[i..]
}

#[cfg(test)]
mod tests {
    use super::*;

    // Minimal byte fixtures — magic-byte signatures are stable so we
    // don't need real images.
    const PNG_HEADER: &[u8] = &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    const JPEG_HEADER: &[u8] = &[0xFF, 0xD8, 0xFF, 0xE0, 0, 0x10, b'J', b'F', b'I', b'F'];
    const WEBP_HEADER: &[u8] = b"RIFF\0\0\0\0WEBPVP8 ";
    const SVG_BODY: &[u8] = b"<svg xmlns=\"http://www.w3.org/2000/svg\"/>";

    fn pad(prefix: &[u8], total: usize) -> Vec<u8> {
        let mut v = prefix.to_vec();
        v.resize(total, 0);
        v
    }

    #[test]
    fn accepts_matching_png() {
        let bytes = pad(PNG_HEADER, 64);
        validate_image_bytes(&bytes, ImageContentType::Png, 1024).expect("ok");
    }

    #[test]
    fn accepts_matching_jpeg() {
        let bytes = pad(JPEG_HEADER, 64);
        validate_image_bytes(&bytes, ImageContentType::Jpeg, 1024).expect("ok");
    }

    #[test]
    fn accepts_matching_webp() {
        let bytes = pad(WEBP_HEADER, 64);
        validate_image_bytes(&bytes, ImageContentType::Webp, 1024).expect("ok");
    }

    #[test]
    fn accepts_svg_text() {
        validate_image_bytes(SVG_BODY, ImageContentType::Svg, 1024).expect("ok");
    }

    #[test]
    fn accepts_svg_with_xml_prolog() {
        let body: &[u8] = b"<?xml version=\"1.0\"?><svg xmlns=\"http://www.w3.org/2000/svg\"/>";
        validate_image_bytes(body, ImageContentType::Svg, 1024).expect("ok");
    }

    #[test]
    fn rejects_png_claimed_as_jpeg() {
        let bytes = pad(PNG_HEADER, 64);
        assert!(matches!(
            validate_image_bytes(&bytes, ImageContentType::Jpeg, 1024),
            Err(AssetError::MagicByteMismatch { .. })
        ));
    }

    #[test]
    fn rejects_svg_claimed_as_png() {
        assert!(matches!(
            validate_image_bytes(SVG_BODY, ImageContentType::Png, 1024),
            Err(AssetError::MagicByteMismatch { .. })
        ));
    }

    #[test]
    fn rejects_unknown_payload() {
        assert!(matches!(
            validate_image_bytes(b"not a known image", ImageContentType::Png, 1024),
            Err(AssetError::UnknownFileType)
        ));
    }

    #[test]
    fn rejects_oversize() {
        let bytes = pad(PNG_HEADER, 2048);
        assert!(matches!(
            validate_image_bytes(&bytes, ImageContentType::Png, 1024),
            Err(AssetError::TooLarge { max: 1024, .. })
        ));
    }

    #[test]
    fn rejects_empty() {
        assert!(matches!(
            validate_image_bytes(b"", ImageContentType::Png, 1024),
            Err(AssetError::UnknownFileType)
        ));
    }
}
