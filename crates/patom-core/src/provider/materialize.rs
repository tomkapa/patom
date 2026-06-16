//! Dispatch-time materialization of attachment references.
//!
//! A stored [`Attachment`](super::attachment::Attachment) carries only a URL.
//! Some provider conversions need the raw bytes: OpenAI inlines PDF as a
//! base64 `file` part, and both providers extract Office documents to text
//! (OpenAI's inline `file_data` is PDF-only; Anthropic has no native Office
//! input). Plain-text files (md/json/toml/…) are UTF-8 decoded. This module
//! provides the async fetch seam plus the document/text materializers, kept out
//! of the per-provider converters so the conversion code stays a pure mapping
//! over already-resolved data.
//!
//! Image and PDF content on the Anthropic path, and image content on OpenAI,
//! ride as URLs and never touch this module — only the byte-requiring cases do.

use std::io::{Cursor, Read};
use std::sync::Arc;

use async_trait::async_trait;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use bytes::Bytes;
use thiserror::Error;

use crate::assets::AssetUrl;
use crate::tools::truncate_to_char_boundary;

use super::attachment::AttachmentMime;
use super::limits::{ATTACHMENT_FETCH_TIMEOUT, MAX_ATTACHMENT_FILE_BYTES, MAX_DOC_EXTRACT_BYTES};

/// Failure materializing an attachment before dispatch.
#[derive(Debug, Error)]
pub enum AttachmentError {
    #[error("fetch failed: {0}")]
    Fetch(String),
    #[error("attachment too large: max {max}, got {got}")]
    TooLarge { max: u64, got: u64 },
    #[error("text extraction failed: {0}")]
    Extract(String),
}

/// Source of attachment bytes, keyed by the public asset URL.
///
/// Injected into providers so tests can supply canned bytes and production
/// fetches from object storage. CLAUDE.md §11-style seam: no ambient I/O in
/// conversion.
#[async_trait]
pub trait AttachmentSource: std::fmt::Debug + Send + Sync {
    /// Fetch the bytes behind `url`, bounded in size and time.
    async fn fetch(&self, url: &AssetUrl) -> Result<Bytes, AttachmentError>;
}

/// Cheap-clone handle to an [`AttachmentSource`].
pub type SharedAttachmentSource = Arc<dyn AttachmentSource>;

/// Production [`AttachmentSource`]: a plain HTTP GET against the public asset URL.
///
/// Patom's asset origin serves these objects publicly (same host the FE
/// renders), so no credentials are needed — only a bounded read.
#[derive(Debug, Clone)]
pub struct HttpAttachmentSource {
    client: reqwest::Client,
}

impl HttpAttachmentSource {
    /// Build a fetcher with the per-request timeout from limits.
    pub fn new() -> Result<Self, AttachmentError> {
        let client = reqwest::Client::builder()
            .timeout(ATTACHMENT_FETCH_TIMEOUT)
            .build()
            .map_err(|e| AttachmentError::Fetch(e.to_string()))?;
        Ok(Self { client })
    }
}

#[async_trait]
impl AttachmentSource for HttpAttachmentSource {
    async fn fetch(&self, url: &AssetUrl) -> Result<Bytes, AttachmentError> {
        // §5: bound the whole I/O round-trip in time *and* size. The client
        // also carries a timeout; the outer wrap guards the body read too.
        let fetch = async {
            let resp = self
                .client
                .get(url.as_str())
                .send()
                .await
                .map_err(|e| AttachmentError::Fetch(e.to_string()))?;
            if !resp.status().is_success() {
                return Err(AttachmentError::Fetch(format!("status {}", resp.status())));
            }
            // Reject on the advertised length before downloading the body.
            if let Some(len) = resp.content_length()
                && len > MAX_ATTACHMENT_FILE_BYTES
            {
                return Err(AttachmentError::TooLarge {
                    max: MAX_ATTACHMENT_FILE_BYTES,
                    got: len,
                });
            }
            let bytes = resp
                .bytes()
                .await
                .map_err(|e| AttachmentError::Fetch(e.to_string()))?;
            // A lying/absent Content-Length is still caught after the read.
            let got = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
            if got > MAX_ATTACHMENT_FILE_BYTES {
                return Err(AttachmentError::TooLarge {
                    max: MAX_ATTACHMENT_FILE_BYTES,
                    got,
                });
            }
            Ok(bytes)
        };
        tokio::time::timeout(ATTACHMENT_FETCH_TIMEOUT, fetch)
            .await
            .map_err(|_| AttachmentError::Fetch("timed out".to_owned()))?
    }
}

/// Encode bytes as an RFC 2397 data URI (`data:{mime};base64,…`). Used for
/// OpenAI `image_url`/`file` parts that inline the payload.
#[must_use]
pub fn to_data_uri(mime: AttachmentMime, bytes: &[u8]) -> String {
    format!("data:{};base64,{}", mime.as_mime(), BASE64.encode(bytes))
}

/// Render a document/text attachment as the text we inject into the prompt.
///
/// Office (xlsx/docx) is parsed, a plain-text file is UTF-8 decoded. Both are
/// capped at [`MAX_DOC_EXTRACT_BYTES`]. Images/PDF are not text — they ride as
/// their own provider parts and never reach here.
pub fn attachment_to_text(mime: AttachmentMime, bytes: &[u8]) -> Result<String, AttachmentError> {
    let mut text = match mime {
        AttachmentMime::Xlsx => extract_xlsx(bytes)?,
        AttachmentMime::Docx => extract_docx(bytes)?,
        AttachmentMime::Text => decode_text(bytes),
        other => {
            return Err(AttachmentError::Extract(format!(
                "not a text-renderable type: {}",
                other.as_mime()
            )));
        }
    };
    truncate_to_char_boundary(&mut text, MAX_DOC_EXTRACT_BYTES);
    Ok(text)
}

/// Decode a plain-text file as UTF-8. Lossy so a stray invalid byte yields a
/// replacement char rather than failing the turn — the upload boundary already
/// rejected binary payloads (NUL check).
fn decode_text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// Flatten every sheet into tab-separated rows prefixed by the sheet name.
fn extract_xlsx(bytes: &[u8]) -> Result<String, AttachmentError> {
    use calamine::{Reader, Xlsx};

    // `Cursor<&[u8]>` is `Read + Seek`, which is all `calamine` needs — no
    // need to copy the (up to 32 MiB) file into an owned buffer.
    let cursor = Cursor::new(bytes);
    let mut workbook =
        Xlsx::new(cursor).map_err(|e| AttachmentError::Extract(format!("xlsx open: {e}")))?;

    let mut out = String::new();
    // `sheet_names` returns an owned `Vec<String>`, so iterating it does not
    // borrow `workbook` and leaves `worksheet_range`'s `&mut self` free.
    let names = workbook.sheet_names();
    // Bounded by the number of sheets in the workbook (§5).
    for name in names {
        if out.len() >= MAX_DOC_EXTRACT_BYTES {
            break;
        }
        let Ok(range) = workbook.worksheet_range(&name) else {
            continue;
        };
        out.push_str("# ");
        out.push_str(&name);
        out.push('\n');
        for row in range.rows() {
            let cells: Vec<String> = row.iter().map(ToString::to_string).collect();
            out.push_str(&cells.join("\t"));
            out.push('\n');
            if out.len() >= MAX_DOC_EXTRACT_BYTES {
                break;
            }
        }
    }
    Ok(out)
}

/// Unzip `word/document.xml` and scan its runs into plain text.
fn extract_docx(bytes: &[u8]) -> Result<String, AttachmentError> {
    let cursor = Cursor::new(bytes);
    let mut archive = zip::ZipArchive::new(cursor)
        .map_err(|e| AttachmentError::Extract(format!("docx zip: {e}")))?;
    let mut xml = String::new();
    {
        let mut entry = archive
            .by_name("word/document.xml")
            .map_err(|e| AttachmentError::Extract(format!("docx body: {e}")))?;
        entry
            .read_to_string(&mut xml)
            .map_err(|e| AttachmentError::Extract(format!("docx read: {e}")))?;
    }
    Ok(docx_xml_to_text(&xml))
}

/// Minimal OOXML word-processing text scan: emit the text inside `<w:t>` runs,
/// a tab for `<w:tab/>`, and a newline at each paragraph (`</w:p>`) and break
/// (`<w:br/>`). Bounded by the input length (§5). Not a general XML parser —
/// it understands exactly the handful of elements that carry visible text.
fn docx_xml_to_text(xml: &str) -> String {
    let mut out = String::new();
    let mut inside_text = false;
    let mut rest = xml;
    let max_steps = xml.len() + 1;
    let mut steps = 0usize;
    loop {
        steps += 1;
        assert!(steps <= max_steps, "docx scan is bounded by input length");
        let Some(lt) = rest.find('<') else {
            break;
        };
        if inside_text {
            push_unescaped(&mut out, &rest[..lt]);
        }
        let after = &rest[lt..];
        let Some(gt_off) = after.find('>') else {
            break;
        };
        let tag = &after[1..gt_off]; // between '<' and '>'
        let name = tag_name(tag);
        match name {
            "w:t" => inside_text = true,
            "/w:t" => inside_text = false,
            "w:tab" => out.push('\t'),
            // Line break, carriage return, and end-of-paragraph all render as a newline.
            "w:br" | "w:cr" | "/w:p" => out.push('\n'),
            _ => {}
        }
        rest = &after[gt_off + 1..];
    }
    out
}

/// Element name from a raw tag body (without the angle brackets): the leading
/// token up to whitespace or a self-closing `/`. A leading `/` (closing tag) is
/// kept; a trailing self-close `/` is dropped.
fn tag_name(tag: &str) -> &str {
    let stop = |c: char| c.is_whitespace() || c == '/';
    // Scan the body after any leading '/', then re-add that '/' to the length
    // so a closing tag keeps its slash.
    let slash = usize::from(tag.starts_with('/'));
    let body = &tag[slash..];
    let end = body.find(stop).unwrap_or(body.len());
    &tag[..slash + end]
}

/// Append `s` to `out`, decoding the five predefined XML entities.
fn push_unescaped(out: &mut String, s: &str) {
    if !s.contains('&') {
        out.push_str(s);
        return;
    }
    let mut rest = s;
    let max_steps = s.len() + 1;
    let mut steps = 0usize;
    loop {
        steps += 1;
        assert!(steps <= max_steps, "entity scan is bounded by input length");
        let Some(amp) = rest.find('&') else {
            out.push_str(rest);
            break;
        };
        out.push_str(&rest[..amp]);
        let tail = &rest[amp..];
        let decoded = if let Some(after) = tail.strip_prefix("&amp;") {
            out.push('&');
            after
        } else if let Some(after) = tail.strip_prefix("&lt;") {
            out.push('<');
            after
        } else if let Some(after) = tail.strip_prefix("&gt;") {
            out.push('>');
            after
        } else if let Some(after) = tail.strip_prefix("&quot;") {
            out.push('"');
            after
        } else if let Some(after) = tail.strip_prefix("&apos;") {
            out.push('\'');
            after
        } else {
            out.push('&');
            &tail[1..]
        };
        rest = decoded;
    }
}

// `pub(crate)` is intentional and not redundant: the items are reached from
// sibling modules' tests (e.g. `provider::openai::convert`), so they must be
// crate-visible — but `pub` would be `unreachable_pub` in this private-ish
// test module. The two nursery lints conflict; pin the correct visibility.
#[cfg(test)]
#[allow(clippy::redundant_pub_crate)]
pub(crate) mod test_support {
    use super::*;
    use std::collections::HashMap;

    /// Test [`AttachmentSource`] returning canned bytes per URL. A miss is a
    /// `Fetch` error, so tests asserting "no fetch happened" can register
    /// nothing and still pass for URL-only conversion paths.
    #[derive(Debug, Default)]
    pub(crate) struct StubAttachmentSource {
        items: HashMap<String, Bytes>,
    }

    impl StubAttachmentSource {
        pub(crate) fn new() -> Self {
            Self::default()
        }

        pub(crate) fn with(mut self, url: &str, bytes: Vec<u8>) -> Self {
            self.items.insert(url.to_owned(), Bytes::from(bytes));
            self
        }
    }

    #[async_trait]
    impl AttachmentSource for StubAttachmentSource {
        async fn fetch(&self, url: &AssetUrl) -> Result<Bytes, AttachmentError> {
            self.items
                .get(url.as_str())
                .cloned()
                .ok_or_else(|| AttachmentError::Fetch(format!("no stub for {url}")))
        }
    }

    /// Build a tiny valid `.docx` (a ZIP holding `word/document.xml` with one
    /// run) for converter tests that exercise the Office → text path.
    pub(crate) fn tiny_docx(body: &str) -> Vec<u8> {
        use std::io::Write as _;
        use zip::write::SimpleFileOptions;

        let mut buf = Vec::new();
        {
            let mut zip = zip::ZipWriter::new(Cursor::new(&mut buf));
            zip.start_file("word/document.xml", SimpleFileOptions::default())
                .expect("start");
            let xml = format!(
                "<w:document><w:body><w:p><w:r><w:t>{body}</w:t></w:r></w:p></w:body></w:document>"
            );
            zip.write_all(xml.as_bytes()).expect("write");
            zip.finish().expect("finish");
        }
        buf
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn data_uri_has_mime_and_base64() {
        let uri = to_data_uri(AttachmentMime::Png, b"hello");
        assert!(uri.starts_with("data:image/png;base64,"));
        assert!(uri.ends_with("aGVsbG8="));
    }

    #[test]
    fn docx_scan_extracts_runs_and_paragraph_breaks() {
        let xml = r#"<w:document><w:body>
            <w:p><w:r><w:t>Hello</w:t></w:r><w:r><w:t xml:space="preserve"> world</w:t></w:r></w:p>
            <w:p><w:r><w:t>Line &amp; two</w:t></w:r></w:p>
            </w:body></w:document>"#;
        let text = docx_xml_to_text(xml);
        assert!(text.contains("Hello world"), "got: {text:?}");
        assert!(text.contains("Line & two"), "entity decode: {text:?}");
        // Two paragraphs → at least two newlines.
        assert!(text.matches('\n').count() >= 2);
    }

    #[test]
    fn docx_scan_handles_tab_and_self_closing() {
        let xml = "<w:p><w:r><w:t>a</w:t><w:tab/><w:t>b</w:t></w:r></w:p>";
        let text = docx_xml_to_text(xml);
        assert_eq!(text, "a\tb\n");
    }

    #[test]
    fn extract_office_rejects_non_office() {
        assert!(attachment_to_text(AttachmentMime::Pdf, b"%PDF").is_err());
    }
}
