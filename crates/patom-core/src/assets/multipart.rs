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
use super::traits::{AssetContentType, AssetKind, ImageContentType};

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

/// One message attachment extracted from a multipart body (issue #187).
#[derive(Debug)]
pub struct UploadedAttachment {
    pub bytes: Bytes,
    pub content_type: AssetContentType,
    /// The original filename from the part's Content-Disposition.
    pub filename: String,
}

/// Read one message attachment from a multipart body.
///
/// Like [`extract_single_image_field`] but for the broader attachment
/// allow-list (images + PDF + Office), capturing the original filename and
/// applying the per-type size cap. SVG is rejected (not valid model input).
pub async fn extract_attachment_field(
    mut multipart: Multipart,
) -> Result<UploadedAttachment, AssetError> {
    let field = with_timeout(multipart.next_field())
        .await?
        .ok_or(AssetError::MissingField)?;
    if field.name() != Some("file") {
        return Err(AssetError::MissingField);
    }
    let filename = field
        .file_name()
        .map(ToOwned::to_owned)
        .ok_or(AssetError::MissingFilename)?;
    let claimed = field
        .content_type()
        .and_then(AssetContentType::from_attachment_mime)
        .ok_or_else(|| {
            AssetError::ContentTypeNotAllowed(
                field.content_type().unwrap_or("<missing>").to_owned(),
            )
        })?;

    let bytes = with_timeout(field.bytes()).await?;

    // Exactly one field, same as the avatar path.
    if with_timeout(multipart.next_field()).await?.is_some() {
        return Err(AssetError::TooManyFields);
    }

    validate_attachment_bytes(&bytes, claimed, claimed.attachment_max_bytes())?;

    Ok(UploadedAttachment {
        bytes,
        content_type: claimed,
        filename,
    })
}

/// Size + magic-byte cross-check for a message attachment.
///
/// Images reuse the `infer` sniff; PDF and Office check their container
/// signatures (`%PDF-`, the OOXML ZIP `PK\x03\x04`). Distinguishing xlsx from
/// docx is left to the downstream parser/provider — both are valid ZIP
/// containers here, so the boundary's job is to reject a non-container
/// masquerading as one.
pub fn validate_attachment_bytes(
    bytes: &[u8],
    claimed: AssetContentType,
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
    if !attachment_magic_matches(prefix, claimed) {
        return Err(AssetError::MagicByteMismatch {
            claimed: claimed.as_mime(),
        });
    }
    Ok(())
}

/// Whether `prefix` matches the claimed attachment content type.
fn attachment_magic_matches(prefix: &[u8], claimed: AssetContentType) -> bool {
    match claimed {
        AssetContentType::Png => matches_image(prefix, "image/png"),
        AssetContentType::Jpeg => matches_image(prefix, "image/jpeg"),
        AssetContentType::Webp => matches_image(prefix, "image/webp"),
        AssetContentType::Gif => matches_image(prefix, "image/gif"),
        // SVG is not an accepted attachment type; never reached via
        // `from_attachment_mime`, but fail closed if it ever is.
        AssetContentType::Svg => false,
        AssetContentType::Pdf => prefix.starts_with(b"%PDF-"),
        // xlsx/docx are OOXML ZIP containers ("PK\x03\x04"). An empty
        // (PK\x05\x06) or spanned (PK\x07\x08) archive is not a real
        // document, so we require the local-file-header signature.
        AssetContentType::Xlsx | AssetContentType::Docx | AssetContentType::Pptx => {
            prefix.starts_with(b"PK\x03\x04")
        }
        // Text has no magic bytes; require the prefix to be valid UTF-8 text so
        // a binary payload can't masquerade as a text file.
        AssetContentType::Text => looks_like_text(prefix),
    }
}

/// Whether `prefix` is plausibly UTF-8 text: no NUL byte, and the bytes decode
/// as UTF-8 except possibly for a codepoint truncated at the prefix boundary.
fn looks_like_text(prefix: &[u8]) -> bool {
    if prefix.contains(&0) {
        return false;
    }
    match std::str::from_utf8(prefix) {
        Ok(_) => true,
        // `error_len() == None` means the only problem is an incomplete final
        // codepoint (the prefix cut mid-character) — still valid text.
        Err(e) => e.error_len().is_none(),
    }
}

/// `infer`-based check that `prefix` is the given image mime.
fn matches_image(prefix: &[u8], mime: &str) -> bool {
    infer::get(prefix).is_some_and(|t| t.mime_type() == mime)
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

    const PDF_HEADER: &[u8] = b"%PDF-1.7\n%\xE2\xE3\xCF\xD3";
    const GIF_HEADER: &[u8] = b"GIF89a\x01\x00\x01\x00";
    const ZIP_HEADER: &[u8] = b"PK\x03\x04\x14\x00\x06\x00";

    #[test]
    fn attachment_accepts_pdf() {
        validate_attachment_bytes(PDF_HEADER, AssetContentType::Pdf, 1024).expect("pdf ok");
    }

    #[test]
    fn attachment_accepts_office_zip_for_xlsx_and_docx() {
        validate_attachment_bytes(ZIP_HEADER, AssetContentType::Xlsx, 1024).expect("xlsx ok");
        validate_attachment_bytes(ZIP_HEADER, AssetContentType::Docx, 1024).expect("docx ok");
    }

    #[test]
    fn attachment_accepts_gif_and_png() {
        validate_attachment_bytes(GIF_HEADER, AssetContentType::Gif, 1024).expect("gif ok");
        let png = pad(PNG_HEADER, 64);
        validate_attachment_bytes(&png, AssetContentType::Png, 1024).expect("png ok");
    }

    #[test]
    fn attachment_rejects_pdf_claimed_as_xlsx() {
        assert!(matches!(
            validate_attachment_bytes(PDF_HEADER, AssetContentType::Xlsx, 1024),
            Err(AssetError::MagicByteMismatch { .. })
        ));
    }

    #[test]
    fn attachment_rejects_executable_claimed_as_pdf() {
        assert!(matches!(
            validate_attachment_bytes(b"MZ\x90\x00 not a pdf", AssetContentType::Pdf, 1024),
            Err(AssetError::MagicByteMismatch { .. })
        ));
    }

    #[test]
    fn attachment_rejects_oversize() {
        let big = pad(PDF_HEADER, 2048);
        assert!(matches!(
            validate_attachment_bytes(&big, AssetContentType::Pdf, 1024),
            Err(AssetError::TooLarge { max: 1024, .. })
        ));
    }

    #[test]
    fn attachment_accepts_utf8_text() {
        validate_attachment_bytes(b"# Notes\nkey = 1\n", AssetContentType::Text, 1024)
            .expect("text ok");
        // A truncated final codepoint (prefix cut mid-char) is still text.
        validate_attachment_bytes(&[b'h', b'i', 0xE2, 0x82], AssetContentType::Text, 1024)
            .expect("truncated codepoint ok");
    }

    #[test]
    fn attachment_rejects_binary_claimed_as_text() {
        assert!(matches!(
            validate_attachment_bytes(b"\x00\x01\x02binary", AssetContentType::Text, 1024),
            Err(AssetError::MagicByteMismatch { .. })
        ));
    }

    #[test]
    fn attachment_mime_strings_match_provider_strings() {
        // Cross-layer drift guard: the asset-storage mime strings must equal
        // the provider-side `AttachmentMime` strings, since the upload returns
        // one and `/prompts` re-parses it through the other.
        use crate::provider::AttachmentMime;
        let pairs = [
            (AssetContentType::Png, AttachmentMime::Png),
            (AssetContentType::Jpeg, AttachmentMime::Jpeg),
            (AssetContentType::Webp, AttachmentMime::Webp),
            (AssetContentType::Gif, AttachmentMime::Gif),
            (AssetContentType::Pdf, AttachmentMime::Pdf),
            (AssetContentType::Xlsx, AttachmentMime::Xlsx),
            (AssetContentType::Docx, AttachmentMime::Docx),
            (AssetContentType::Text, AttachmentMime::Text),
        ];
        for (asset, provider) in pairs {
            assert_eq!(asset.as_mime(), provider.as_mime());
        }
    }

    #[test]
    fn attachment_byte_caps_match_provider_caps() {
        // The asset-layer caps mirror the provider-layer caps (the two cannot
        // share a constant without an `assets → provider` cycle). Guard the
        // hand-sync so a bump on one side can't silently diverge.
        use crate::assets::limits as a;
        use crate::provider::limits as p;
        assert_eq!(
            p::MAX_ATTACHMENT_IMAGE_BYTES,
            u64::try_from(a::MAX_ATTACHMENT_IMAGE_BYTES).expect("usize fits u64"),
        );
        assert_eq!(
            p::MAX_ATTACHMENT_FILE_BYTES,
            u64::try_from(a::MAX_ATTACHMENT_FILE_BYTES).expect("usize fits u64"),
        );
    }
}
