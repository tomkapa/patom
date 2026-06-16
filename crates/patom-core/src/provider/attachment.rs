//! Multimodal attachment references carried in a user message.
//!
//! An [`Attachment`] is a *reference*, not bytes: the uploaded image/file lives
//! in object storage and the message envelope keeps only `{url, mime, filename,
//! size}`. This keeps the `thread_messages.body` JSONB small (it is re-read on
//! every thread render and every turn-context build) while still letting each
//! provider materialise the bytes it needs at dispatch time.
//!
//! CLAUDE.md §1: every value with an invariant is a newtype, parsed once at the
//! boundary via `TryFrom`. The wire shape [`RawAttachment`] is the *only* way
//! in; serde funnels through the same smart constructor.

use serde::{Deserialize, Serialize};

use crate::assets::AssetUrl;
use crate::types::ParseError;

use super::limits::{MAX_ATTACHMENT_FILE_BYTES, MAX_ATTACHMENT_IMAGE_BYTES, MAX_FILENAME_LEN};

/// Content type accepted as model input.
///
/// A sum type, not a `&str`, so exhaustive `match` proves every capability
/// gate and every provider conversion covers each variant (CLAUDE.md §1). The
/// allow-list is exactly what at least one supported provider accepts as input
/// (see issue #187): images everywhere, PDF on Anthropic + OpenAI, Office on
/// both via server-side text extraction (OpenAI's inline `file_data` is
/// PDF-only, so xlsx/docx are extracted rather than sent natively).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttachmentMime {
    Png,
    Jpeg,
    Webp,
    Gif,
    Pdf,
    /// Excel `.xlsx` (OOXML spreadsheet).
    Xlsx,
    /// Word `.docx` (OOXML word-processing document).
    Docx,
    /// Any UTF-8 text file (`.md`, `.json`, `.toml`, `.txt`, `.csv`, `.yaml`,
    /// `.xml`, source code, …). Delivered to every provider — including
    /// text-only DeepSeek — as the decoded text.
    Text,
}

impl AttachmentMime {
    /// Canonical wire-form `Content-Type` value.
    #[must_use]
    pub const fn as_mime(self) -> &'static str {
        match self {
            Self::Png => "image/png",
            Self::Jpeg => "image/jpeg",
            Self::Webp => "image/webp",
            Self::Gif => "image/gif",
            Self::Pdf => "application/pdf",
            Self::Xlsx => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
            Self::Docx => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
            Self::Text => "text/plain",
        }
    }

    /// Canonical file extension (no leading dot). Text files normalise to
    /// `txt` for the storage key; the original name (with its real extension)
    /// is preserved separately in the [`FileName`].
    #[must_use]
    pub const fn extension(self) -> &'static str {
        match self {
            Self::Png => "png",
            Self::Jpeg => "jpg",
            Self::Webp => "webp",
            Self::Gif => "gif",
            Self::Pdf => "pdf",
            Self::Xlsx => "xlsx",
            Self::Docx => "docx",
            Self::Text => "txt",
        }
    }

    /// Parse a wire-form `Content-Type` value, ignoring any `; charset=…`
    /// parameter. Returns `None` for anything outside the allow-list.
    #[must_use]
    pub fn from_mime(raw: &str) -> Option<Self> {
        let canonical = raw
            .split(';')
            .next()
            .unwrap_or(raw)
            .trim()
            .to_ascii_lowercase();
        match canonical.as_str() {
            "image/png" => Some(Self::Png),
            "image/jpeg" | "image/jpg" => Some(Self::Jpeg),
            "image/webp" => Some(Self::Webp),
            "image/gif" => Some(Self::Gif),
            "application/pdf" => Some(Self::Pdf),
            "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet" => Some(Self::Xlsx),
            "application/vnd.openxmlformats-officedocument.wordprocessingml.document" => {
                Some(Self::Docx)
            }
            // Any text/* type (markdown, csv, html, yaml, plain, …) plus the
            // common `application/*` text formats are treated as plain text.
            "application/json"
            | "application/toml"
            | "application/x-toml"
            | "application/xml"
            | "application/yaml"
            | "application/x-yaml"
            | "application/x-ndjson" => Some(Self::Text),
            other if other.starts_with("text/") => Some(Self::Text),
            _ => None,
        }
    }

    /// Whether this is an image type (native on every multimodal provider).
    #[must_use]
    pub const fn is_image(self) -> bool {
        matches!(self, Self::Png | Self::Jpeg | Self::Webp | Self::Gif)
    }

    /// Whether this is PDF (native `document` block on Anthropic, `file` part
    /// on OpenAI).
    #[must_use]
    pub const fn is_pdf(self) -> bool {
        matches!(self, Self::Pdf)
    }

    /// Whether this is an Office format (xlsx/docx). Both providers receive it
    /// as server-side extracted text — OpenAI's inline `file_data` is PDF-only,
    /// and Anthropic has no native Office input.
    #[must_use]
    pub const fn is_office(self) -> bool {
        matches!(self, Self::Xlsx | Self::Docx)
    }

    /// Whether this is a plain-text file. Delivered to every provider as the
    /// decoded UTF-8 text (no parsing, no native part).
    #[must_use]
    pub const fn is_text(self) -> bool {
        matches!(self, Self::Text)
    }

    /// Per-mime decoded-size ceiling. Images and files have different caps
    /// because files inline as base64 against the OpenAI 50 MB/file limit.
    #[must_use]
    pub const fn max_bytes(self) -> u64 {
        if self.is_image() {
            MAX_ATTACHMENT_IMAGE_BYTES
        } else {
            MAX_ATTACHMENT_FILE_BYTES
        }
    }
}

/// An attachment's original filename. Display-only metadata, bounded and free
/// of path separators so it can never be interpreted as a storage path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileName(String);

impl FileName {
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<&str> for FileName {
    type Error = ParseError;

    fn try_from(raw: &str) -> Result<Self, Self::Error> {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Err(ParseError::Empty {
                field: "attachment.filename",
            });
        }
        if trimmed.len() > MAX_FILENAME_LEN {
            return Err(ParseError::TooLong {
                field: "attachment.filename",
                max: MAX_FILENAME_LEN,
                got: trimmed.len(),
            });
        }
        // Reject anything that could be read back as a path: separators, NUL,
        // or other control characters.
        if trimmed.contains(['/', '\\']) || trimmed.contains(|c: char| c.is_control()) {
            return Err(ParseError::Malformed {
                field: "attachment.filename",
                detail: "must not contain path separators or control characters",
            });
        }
        Ok(Self(trimmed.to_owned()))
    }
}

/// A validated reference to an uploaded attachment.
///
/// Carries no bytes — providers fetch them on demand. Constructed only via
/// [`TryFrom<RawAttachment>`]; fields are private and read through getters.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "RawAttachment", into = "RawAttachment")]
pub struct Attachment {
    url: AssetUrl,
    mime: AttachmentMime,
    filename: FileName,
    size: u64,
}

impl Attachment {
    #[must_use]
    pub fn url(&self) -> &AssetUrl {
        &self.url
    }

    #[must_use]
    pub const fn mime(&self) -> AttachmentMime {
        self.mime
    }

    #[must_use]
    pub fn filename(&self) -> &FileName {
        &self.filename
    }

    /// Reported decoded size in bytes. Validated `> 0` and `<= mime.max_bytes()`
    /// at construction.
    #[must_use]
    pub const fn size(&self) -> u64 {
        self.size
    }
}

/// Wire / storage shape for an [`Attachment`]. This is the deserialization
/// funnel: every `Attachment` value enters through [`Attachment::try_from`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RawAttachment {
    pub url: String,
    pub mime: String,
    pub filename: String,
    pub size: u64,
}

impl TryFrom<RawAttachment> for Attachment {
    type Error = ParseError;

    fn try_from(raw: RawAttachment) -> Result<Self, Self::Error> {
        let url = AssetUrl::try_from(raw.url.as_str())?;
        let mime = AttachmentMime::from_mime(&raw.mime).ok_or(ParseError::Malformed {
            field: "attachment.mime",
            detail: "unsupported content type",
        })?;
        let filename = FileName::try_from(raw.filename.as_str())?;
        if raw.size == 0 {
            return Err(ParseError::OutOfRange {
                field: "attachment.size",
                detail: "must be greater than zero",
            });
        }
        if raw.size > mime.max_bytes() {
            return Err(ParseError::OutOfRange {
                field: "attachment.size",
                detail: "exceeds the per-type byte cap",
            });
        }
        Ok(Self {
            url,
            mime,
            filename,
            size: raw.size,
        })
    }
}

impl From<Attachment> for RawAttachment {
    fn from(a: Attachment) -> Self {
        Self {
            url: a.url.as_str().to_owned(),
            mime: a.mime.as_mime().to_owned(),
            filename: a.filename.as_str().to_owned(),
            size: a.size,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(mime: &str, size: u64) -> RawAttachment {
        RawAttachment {
            url: "https://assets.example/attachments/abc.png".to_owned(),
            mime: mime.to_owned(),
            filename: "photo.png".to_owned(),
            size,
        }
    }

    #[test]
    fn mime_round_trips_via_string() {
        for m in [
            AttachmentMime::Png,
            AttachmentMime::Jpeg,
            AttachmentMime::Webp,
            AttachmentMime::Gif,
            AttachmentMime::Pdf,
            AttachmentMime::Xlsx,
            AttachmentMime::Docx,
        ] {
            assert_eq!(AttachmentMime::from_mime(m.as_mime()), Some(m));
        }
    }

    #[test]
    fn mime_classifiers_partition_the_allowlist() {
        for m in [AttachmentMime::Png, AttachmentMime::Gif] {
            assert!(m.is_image() && !m.is_pdf() && !m.is_office());
        }
        assert!(AttachmentMime::Pdf.is_pdf() && !AttachmentMime::Pdf.is_image());
        for m in [AttachmentMime::Xlsx, AttachmentMime::Docx] {
            assert!(m.is_office() && !m.is_image() && !m.is_pdf());
        }
    }

    #[test]
    fn mime_normalises_charset_and_jpg_alias() {
        assert_eq!(
            AttachmentMime::from_mime("image/jpg"),
            Some(AttachmentMime::Jpeg)
        );
        assert_eq!(
            AttachmentMime::from_mime("application/pdf; charset=binary"),
            Some(AttachmentMime::Pdf)
        );
        // Text formats (incl. any text/* and common application/* text types)
        // map to `Text`; truly-unsupported binary types do not.
        assert_eq!(
            AttachmentMime::from_mime("text/markdown; charset=utf-8"),
            Some(AttachmentMime::Text)
        );
        assert_eq!(
            AttachmentMime::from_mime("application/toml"),
            Some(AttachmentMime::Text)
        );
        assert_eq!(AttachmentMime::from_mime("application/octet-stream"), None);
        assert_eq!(AttachmentMime::from_mime("audio/mpeg"), None);
    }

    #[test]
    fn filename_rejects_path_separators_and_control() {
        assert!(FileName::try_from("../etc/passwd").is_err());
        assert!(FileName::try_from("a\\b.docx").is_err());
        assert!(FileName::try_from("bad\u{0}name").is_err());
        assert!(FileName::try_from("").is_err());
    }

    #[test]
    fn filename_accepts_normal() {
        let f = FileName::try_from("Q3 report (final).xlsx").expect("valid");
        assert_eq!(f.as_str(), "Q3 report (final).xlsx");
    }

    #[test]
    fn attachment_rejects_unsupported_mime() {
        let err = Attachment::try_from(raw("application/octet-stream", 100)).expect_err("rejected");
        assert!(matches!(err, ParseError::Malformed { .. }));
    }

    #[test]
    fn attachment_rejects_zero_and_oversize() {
        assert!(matches!(
            Attachment::try_from(raw("image/png", 0)),
            Err(ParseError::OutOfRange { .. })
        ));
        let over = MAX_ATTACHMENT_IMAGE_BYTES + 1;
        assert!(matches!(
            Attachment::try_from(raw("image/png", over)),
            Err(ParseError::OutOfRange { .. })
        ));
    }

    #[test]
    fn attachment_size_cap_is_per_mime() {
        // A size legal for files is illegal for images.
        let big = MAX_ATTACHMENT_IMAGE_BYTES + 1;
        assert!(big <= MAX_ATTACHMENT_FILE_BYTES);
        assert!(Attachment::try_from(raw("image/png", big)).is_err());
        assert!(Attachment::try_from(raw("application/pdf", big)).is_ok());
    }

    #[test]
    fn attachment_json_round_trips_through_smart_constructor() {
        let a = Attachment::try_from(raw("image/png", 2048)).expect("valid");
        let json = serde_json::to_string(&a).expect("ser");
        let back: Attachment = serde_json::from_str(&json).expect("de");
        assert_eq!(back, a);
        assert_eq!(back.mime(), AttachmentMime::Png);
        assert_eq!(back.size(), 2048);
        assert_eq!(back.filename().as_str(), "photo.png");
    }

    #[test]
    fn attachment_deserialize_rejects_bad_url() {
        let bad = RawAttachment {
            url: "/relative/x.png".to_owned(),
            mime: "image/png".to_owned(),
            filename: "x.png".to_owned(),
            size: 10,
        };
        let json = serde_json::to_string(&bad).expect("ser");
        assert!(serde_json::from_str::<Attachment>(&json).is_err());
    }
}
