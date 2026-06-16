//! Bidirectional conversion between provider-agnostic chat types and `claudius` types.
//!
//! Kept in its own module so each direction sits in one place and the cognitive load of
//! adding a new content variant is bounded.

use claudius::{
    ContentBlock, DocumentBlock, DocumentSource, ImageBlock, MessageParam, MessageRole, Model,
    StopReason as ClaudiusStop, TextBlock, ToolParam, ToolResultBlock, ToolUnionParam,
    ToolUseBlock, UrlImageSource, UrlPdfSource,
};

use crate::provider::ModelCapabilities;
use crate::provider::attachment::Attachment;
use crate::provider::chat::{
    AssistantContent, ChatMessage, StopReason, ToolCall, ToolCallId, ToolSpec, UserContent,
};
use crate::provider::error::ProviderError;
use crate::provider::materialize::{AttachmentSource, attachment_to_text};
use crate::types::ToolName;

/// Map a provider-agnostic message into a claudius `MessageParam`. Returns `None` for a
/// message that would serialize to an empty block list — Anthropic rejects an
/// empty-content message, so the caller drops it from the replayed history.
///
/// Async + fallible because user attachments may need materializing (Office →
/// extracted text) and gating: content the model cannot accept yields
/// [`ProviderError::UnsupportedContent`] before dispatch (issue #187).
pub(super) async fn message_to_param(
    msg: ChatMessage,
    caps: ModelCapabilities,
    model: &str,
    source: &dyn AttachmentSource,
) -> Result<Option<MessageParam>, ProviderError> {
    match msg {
        ChatMessage::User(content) => {
            let mut blocks = Vec::with_capacity(content.len());
            // One block per content item; `for` (not `map`) because each step awaits
            // (Office extraction) and may early-return on an unsupported kind.
            for c in content {
                blocks.push(user_content_to_block(c, caps, model, source).await?);
            }
            Ok(Some(MessageParam::new_with_blocks(
                blocks,
                MessageRole::User,
            )))
        }
        ChatMessage::Assistant(content) => {
            let blocks: Vec<ContentBlock> = content
                .into_iter()
                .filter_map(assistant_content_to_block)
                .collect();
            // Reasoning blocks drop out (no Anthropic signature to replay), so a
            // reasoning-only turn leaves an empty block list. Emitting an empty-content
            // assistant message is rejected and wedges the session on every replay, so
            // omit it from the wire entirely. The block stays persisted for audit.
            Ok((!blocks.is_empty())
                .then(|| MessageParam::new_with_blocks(blocks, MessageRole::Assistant)))
        }
    }
}

async fn user_content_to_block(
    c: UserContent,
    caps: ModelCapabilities,
    model: &str,
    source: &dyn AttachmentSource,
) -> Result<ContentBlock, ProviderError> {
    match c {
        UserContent::Text(t) => Ok(ContentBlock::Text(TextBlock::new(t))),
        UserContent::ToolResult(r) => {
            let mut block =
                ToolResultBlock::new(r.call_id.as_str().to_string()).with_string_content(r.output);
            // Anthropic only wants `is_error` set when true — sending `is_error: false`
            // is technically valid but adds noise on the wire and in caching keys.
            if r.is_error {
                block = block.with_error(true);
            }
            Ok(ContentBlock::ToolResult(block))
        }
        // Images ride as a URL source — Anthropic fetches the public asset URL,
        // so no byte download here.
        UserContent::Image(att) => {
            reject_unless(caps.accepts(att.mime()), &att, model)?;
            Ok(ContentBlock::Image(ImageBlock::new_with_url(
                UrlImageSource::new(att.url().as_str().to_owned()),
            )))
        }
        UserContent::File(att) => file_to_block(att, caps, model, source).await,
    }
}

/// Map a non-image file. PDF rides as a URL `document` block (native). Office
/// (no native support) and plain-text files are fetched and rendered to a
/// `TextBlock` — Office is parsed, text is UTF-8 decoded.
async fn file_to_block(
    att: Attachment,
    caps: ModelCapabilities,
    model: &str,
    source: &dyn AttachmentSource,
) -> Result<ContentBlock, ProviderError> {
    reject_unless(caps.accepts(att.mime()), &att, model)?;
    if att.mime().is_pdf() {
        return Ok(ContentBlock::Document(DocumentBlock::new(
            DocumentSource::UrlPdf(UrlPdfSource::new(att.url().as_str().to_owned())),
        )));
    }
    // Office / text: fetch + render to text.
    let bytes = source
        .fetch(att.url())
        .await
        .map_err(|e| ProviderError::Attachment(e.to_string()))?;
    let text = attachment_to_text(att.mime(), &bytes)
        .map_err(|e| ProviderError::Attachment(e.to_string()))?;
    let body = format!("[Attached file: {}]\n{text}", att.filename().as_str());
    Ok(ContentBlock::Text(TextBlock::new(body)))
}

/// Capability gate: turn a `false` into a typed rejection carrying the rejected
/// mime and the model it was routed to.
fn reject_unless(ok: bool, att: &Attachment, model: &str) -> Result<(), ProviderError> {
    if ok {
        return Ok(());
    }
    Err(ProviderError::UnsupportedContent {
        mime: att.mime().as_mime(),
        model: model.to_owned(),
    })
}

/// Reasoning blocks are observability-only on our side; we drop them when re-serializing
/// history back to the provider. Replaying them requires Anthropic-specific signature
/// preservation that does not generalise to other providers — when we add streaming
/// thinking proper, this is the seam to revisit.
fn assistant_content_to_block(c: AssistantContent) -> Option<ContentBlock> {
    match c {
        AssistantContent::Text(t) => Some(ContentBlock::Text(TextBlock::new(t))),
        AssistantContent::ToolCall(call) => Some(ContentBlock::ToolUse(ToolUseBlock::new(
            call.id.as_str().to_string(),
            call.name.as_str().to_string(),
            call.input,
        ))),
        AssistantContent::Reasoning(_) => None,
    }
}

/// Map a tool spec to claudius's tool-union representation.
pub(super) fn tool_spec_to_param(spec: &ToolSpec) -> ToolUnionParam {
    let param = ToolParam::new(spec.name.as_str().to_string(), (*spec.input_schema).clone())
        .with_description(spec.description.to_string());
    ToolUnionParam::CustomTool(param)
}

/// Parse a model identifier. `claudius::Model::from_str` is infallible (it falls back to
/// `Custom`), so this never errors today — kept fallible for forward compatibility if the
/// upstream contract tightens.
pub(super) fn parse_model(raw: &str) -> Model {
    raw.parse::<Model>()
        .unwrap_or_else(|()| Model::Custom(raw.to_string()))
}

/// Lift a claudius response block into the provider-agnostic shape. Returns `None` for
/// content variants we deliberately don't surface yet (server-side tool use, web search
/// results, redacted thinking) — when the agent grows to use them, add the mapping here.
pub(super) fn block_to_assistant(block: ContentBlock) -> Option<AssistantContent> {
    match block {
        ContentBlock::Text(t) => Some(AssistantContent::Text(t.text)),
        ContentBlock::Thinking(t) => Some(AssistantContent::Reasoning(t.thinking)),
        ContentBlock::ToolUse(t) => {
            // A name or id we cannot parse means the upstream sent something we wouldn't
            // have registered — drop it so the agent loop terminates cleanly rather than
            // looping on an unknown tool.
            let name = ToolName::try_from(t.name.as_str()).ok()?;
            let id = ToolCallId::try_from(t.id.as_str()).ok()?;
            Some(AssistantContent::ToolCall(ToolCall {
                id,
                name,
                input: t.input,
            }))
        }
        _ => None,
    }
}

pub(super) fn map_stop_reason(stop: Option<ClaudiusStop>) -> StopReason {
    match stop {
        Some(ClaudiusStop::EndTurn) | None => StopReason::EndTurn,
        Some(ClaudiusStop::ToolUse) => StopReason::ToolUse,
        Some(ClaudiusStop::MaxTokens) => StopReason::MaxTokens,
        Some(other) => StopReason::Other(other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::Model;
    use crate::provider::attachment::RawAttachment;
    use crate::provider::materialize::test_support::StubAttachmentSource;

    const ASSET: &str = "https://assets.example/attachments/x.bin";

    fn anthropic_caps() -> ModelCapabilities {
        Model::try_from("claude-opus-4-7")
            .expect("known")
            .capabilities()
    }

    fn attachment(mime: &str, name: &str) -> Attachment {
        Attachment::try_from(RawAttachment {
            url: ASSET.to_owned(),
            mime: mime.to_owned(),
            filename: name.to_owned(),
            size: 16,
        })
        .expect("valid")
    }

    #[tokio::test]
    async fn reasoning_only_assistant_message_is_dropped() {
        // Reasoning blocks have no Anthropic signature to replay, so a reasoning-only
        // turn would serialize to an empty-content message — which Anthropic rejects and
        // which wedges the session on every replay. It must be omitted from the wire.
        let msg = ChatMessage::Assistant(vec![AssistantContent::Reasoning("secret".into())]);
        let src = StubAttachmentSource::new();
        let out = message_to_param(msg, anthropic_caps(), "claude-opus-4-7", &src)
            .await
            .expect("ok");
        assert!(out.is_none());
    }

    #[tokio::test]
    async fn assistant_message_with_text_is_kept() {
        let msg = ChatMessage::Assistant(vec![AssistantContent::Text("hello".into())]);
        let src = StubAttachmentSource::new();
        let out = message_to_param(msg, anthropic_caps(), "claude-opus-4-7", &src)
            .await
            .expect("ok");
        assert!(out.is_some());
    }

    #[tokio::test]
    async fn image_becomes_url_image_block_without_fetch() {
        // A stub with no registered bytes errors on fetch — so this passing
        // proves the image path takes the URL branch and never downloads.
        let msg = ChatMessage::User(vec![UserContent::Image(attachment("image/png", "a.png"))]);
        let src = StubAttachmentSource::new();
        let param = message_to_param(msg, anthropic_caps(), "claude-opus-4-7", &src)
            .await
            .expect("ok")
            .expect("some");
        let json = serde_json::to_value(&param).expect("ser");
        assert_eq!(json["content"][0]["type"], "image");
        assert_eq!(json["content"][0]["source"]["type"], "url");
        assert_eq!(json["content"][0]["source"]["url"], ASSET);
    }

    #[tokio::test]
    async fn pdf_becomes_url_document_block_without_fetch() {
        let msg = ChatMessage::User(vec![UserContent::File(attachment(
            "application/pdf",
            "r.pdf",
        ))]);
        let src = StubAttachmentSource::new();
        let param = message_to_param(msg, anthropic_caps(), "claude-opus-4-7", &src)
            .await
            .expect("ok")
            .expect("some");
        let json = serde_json::to_value(&param).expect("ser");
        assert_eq!(json["content"][0]["type"], "document");
        assert_eq!(json["content"][0]["source"]["type"], "url");
    }

    #[tokio::test]
    async fn office_is_fetched_and_extracted_to_text() {
        // Minimal valid .docx: a ZIP holding word/document.xml with one run.
        let docx = crate::provider::materialize::test_support::tiny_docx("Hello sheet");
        let msg = ChatMessage::User(vec![UserContent::File(attachment(
            "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
            "memo.docx",
        ))]);
        let src = StubAttachmentSource::new().with(ASSET, docx);
        let param = message_to_param(msg, anthropic_caps(), "claude-opus-4-7", &src)
            .await
            .expect("ok")
            .expect("some");
        let json = serde_json::to_value(&param).expect("ser");
        assert_eq!(json["content"][0]["type"], "text");
        let text = json["content"][0]["text"].as_str().expect("text");
        assert!(text.contains("memo.docx"), "prefix: {text}");
        assert!(text.contains("Hello sheet"), "body: {text}");
    }
}
