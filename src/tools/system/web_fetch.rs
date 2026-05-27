use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use reqwest::Client;
use reqwest::redirect::Policy;
use serde::Deserialize;
use serde_json::{Value, json};
use tracing::instrument;

use crate::types::ToolName;

use super::super::limits::{FETCH_MAX_BODY_BYTES, FETCH_MAX_REDIRECTS, FETCH_TIMEOUT};
use super::super::traits::{Tool, ToolCallContext, ToolError};
use super::super::url::{FetchUrl, UrlError, check_host};

/// Content-type prefixes treated as HTML for the markdown-conversion path.
/// Plain ASCII compare on the lower-cased header value — `text/html; charset=utf-8`
/// matches `text/html`, and `application/xhtml+xml` covers the rare strict case.
const HTML_MIME_PREFIXES: [&str; 2] = ["text/html", "application/xhtml+xml"];

const FETCH_USER_AGENT: &str = concat!("patom-rs/", env!("CARGO_PKG_VERSION"));
const FETCH_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// Fetch the body of a single HTTPS URL.
///
/// SSRF defence is layered: [`FetchUrl`] rejects bad URLs at parse time, and the redirect
/// policy re-checks the destination on every hop so a public URL cannot redirect into an
/// internal target.
#[derive(Debug)]
pub struct WebFetchTool {
    name: ToolName,
    schema: Arc<Value>,
    client: Client,
}

impl WebFetchTool {
    pub fn new() -> Result<Self, reqwest::Error> {
        Ok(Self {
            name: ToolName::try_from("web_fetch").expect("static name is valid"),
            schema: Arc::new(json!({
                "type": "object",
                "properties": {
                    "url": {
                        "type": "string",
                        "description": "Fully-qualified https:// URL to fetch."
                    }
                },
                "required": ["url"],
                "additionalProperties": false
            })),
            client: build_client()?,
        })
    }
}

/// Build a fetch-specific HTTP client. Reqwest doesn't let us mutate redirect policy on
/// an existing client, so the agent's general client and the fetch client are siblings.
/// Cost is one extra connection pool — well worth the SSRF guarantee.
fn build_client() -> Result<Client, reqwest::Error> {
    Client::builder()
        .timeout(FETCH_TIMEOUT)
        .connect_timeout(FETCH_CONNECT_TIMEOUT)
        .user_agent(FETCH_USER_AGENT)
        .https_only(true)
        .redirect(Policy::custom(|attempt| {
            if attempt.previous().len() >= FETCH_MAX_REDIRECTS {
                return attempt.error("too many redirects");
            }
            match attempt.url().host() {
                Some(host) => match check_host(&host) {
                    Ok(()) => attempt.follow(),
                    Err(e) => attempt.error(e),
                },
                None => attempt.error(UrlError::HostMissing),
            }
        }))
        .build()
}

#[derive(Debug, Deserialize)]
struct Input {
    url: String,
}

#[async_trait]
impl Tool for WebFetchTool {
    fn name(&self) -> &ToolName {
        &self.name
    }

    fn description(&self) -> &str {
        "Fetch the contents of a single https:// URL and return the response body \
         (truncated to 200 KB). HTML pages are converted to Markdown; JSON, plain \
         text, and other content types are returned as-is. Use this to read \
         documentation, articles, or any page the user references. URLs to \
         private / loopback / metadata hosts are refused."
    }

    fn input_schema(&self) -> Arc<Value> {
        self.schema.clone()
    }

    fn concurrency_safe(&self) -> bool {
        true
    }

    #[instrument(name = "tool.web_fetch", skip_all, fields(patom.tool = "web_fetch"))]
    async fn execute(&self, input: Value, _ctx: &ToolCallContext) -> Result<String, ToolError> {
        let Input { url } = serde_json::from_value(input)
            .map_err(|e| ToolError::InvalidInput(format!("web_fetch: {e}")))?;
        let url = FetchUrl::try_from(url.as_str())?;

        let response = self.client.get(url.as_str()).send().await?;
        let status = response.status();
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        let bytes = response.bytes().await?;

        // §6: response.bytes() never returns more than the connection saw, but we still
        // assert the slice arithmetic — cheap and defends against API drift.
        let take = bytes.len().min(FETCH_MAX_BODY_BYTES);
        assert!(take <= bytes.len());
        let raw = String::from_utf8_lossy(&bytes[..take]).into_owned();

        if !status.is_success() {
            return Err(ToolError::Upstream {
                status: status.as_u16(),
                body: raw,
            });
        }

        // HTML carries scripts, style, head metadata, classes, data-*. Markdown drops all
        // of it and roughly halves the byte count for typical pages. We convert the
        // (possibly byte-truncated) HTML in place; html5ever is forgiving of mid-tag cuts.
        let body = maybe_html_to_markdown(content_type.as_deref(), raw);

        if bytes.len() > FETCH_MAX_BODY_BYTES {
            Ok(format!(
                "{body}\n\n[truncated to {FETCH_MAX_BODY_BYTES} bytes of {} total]",
                bytes.len()
            ))
        } else {
            Ok(body)
        }
    }
}

/// Convert HTML responses to Markdown so the model isn't billed for `<script>`, `<style>`,
/// inline data attributes, and the rest of the structural noise. Non-HTML content
/// (JSON, plain text, anything else) is returned untouched. On parse failure we fall back
/// to the raw body — better to ship noisy bytes than to fail the tool call.
fn maybe_html_to_markdown(content_type: Option<&str>, raw: String) -> String {
    if !is_html(content_type) {
        return raw;
    }
    // htmd's default emits the *text content* of `<script>` and `<style>` — we don't want
    // either in the model's context. `<noscript>` and `<iframe>` are skipped for the same
    // reason. `<head>` carries metadata only.
    let converter = htmd::HtmlToMarkdown::builder()
        .skip_tags(vec!["script", "style", "noscript", "iframe", "head"])
        .build();
    converter.convert(&raw).unwrap_or(raw)
}

fn is_html(content_type: Option<&str>) -> bool {
    let Some(ct) = content_type else { return false };
    let head = ct
        .split(';')
        .next()
        .unwrap_or(ct)
        .trim()
        .to_ascii_lowercase();
    HTML_MIME_PREFIXES.iter().any(|p| head == *p)
}

#[cfg(test)]
mod tests {
    use super::{is_html, maybe_html_to_markdown};

    #[test]
    fn is_html_detects_text_html_with_charset() {
        assert!(is_html(Some("text/html; charset=utf-8")));
        assert!(is_html(Some("TEXT/HTML")));
        assert!(is_html(Some("application/xhtml+xml")));
    }

    #[test]
    fn is_html_rejects_non_html_mimes() {
        assert!(!is_html(Some("application/json")));
        assert!(!is_html(Some("text/plain")));
        assert!(!is_html(Some("text/html-bogus")));
        assert!(!is_html(None));
    }

    #[test]
    fn html_response_converted_to_markdown() {
        let html = "<html><head><style>body { color: red; }</style></head><body>\
                    <h1>Title</h1><p>hello <b>world</b></p></body></html>";
        let md = maybe_html_to_markdown(Some("text/html; charset=utf-8"), html.to_owned());
        assert!(md.contains("# Title"), "missing h1: {md}");
        assert!(md.contains("**world**"), "missing bold: {md}");
        assert!(!md.contains("<style>"), "html leaked through: {md}");
        assert!(md.len() < html.len(), "markdown should be smaller");
    }

    #[test]
    fn script_and_style_contents_are_stripped() {
        let html = r#"<html><head>
            <script>var __data = { leak: "this should not reach the model" }</script>
            <style>.cls { display: none; color: red }</style>
            </head><body><noscript>js-disabled fallback</noscript>
            <h1>Visible</h1></body></html>"#;
        let md = maybe_html_to_markdown(Some("text/html"), html.to_owned());
        assert!(md.contains("# Visible"), "main content lost: {md}");
        assert!(!md.contains("__data"), "script content leaked: {md}");
        assert!(!md.contains("display:none"), "style content leaked: {md}");
        assert!(!md.contains("js-disabled"), "noscript content leaked: {md}");
    }

    #[test]
    fn non_html_response_passes_through_unchanged() {
        let body = r#"{"k":"v"}"#.to_owned();
        let out = maybe_html_to_markdown(Some("application/json"), body.clone());
        assert_eq!(out, body);
    }

    #[test]
    fn missing_content_type_passes_through_unchanged() {
        let body = "<h1>hi</h1>".to_owned();
        let out = maybe_html_to_markdown(None, body.clone());
        assert_eq!(out, body);
    }

    #[test]
    fn truncated_html_does_not_panic() {
        // Mid-tag cut — html5ever should still produce *something*.
        let html = "<html><body><h1>Title</h1><p>partial cont".to_owned();
        let _ = maybe_html_to_markdown(Some("text/html"), html);
    }
}
