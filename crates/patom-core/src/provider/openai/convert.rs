//! Bidirectional conversion between provider-agnostic chat types and the wire schema we
//! send to OpenAI-Chat-Completions-compatible endpoints.
//!
//! We don't use `async_openai::types::CreateChatCompletionRequest` directly because some
//! compatible endpoints (DeepSeek V4 thinking-mode models) require the assistant's
//! `reasoning_content` to be replayed alongside `tool_calls` on subsequent turns
//! (api-docs.deepseek.com/guides/thinking_mode). The stock OpenAI request schema has no
//! such field, so we define our own typed request body that carries it. Stock OpenAI
//! ignores unknown fields, so the same payload works against both.

use async_openai::types::chat::{
    ChatCompletionMessageToolCall, ChatCompletionMessageToolCalls, ChatCompletionTool,
    ChatCompletionTools, FinishReason, FunctionCall, FunctionObject,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::provider::ModelCapabilities;
use crate::provider::attachment::Attachment;
use crate::provider::chat::{
    AssistantContent, ChatMessage, StopReason, ToolCall, ToolCallId, ToolResult, ToolSpec,
    UserContent,
};
use crate::provider::error::ProviderError;
use crate::provider::materialize::{AttachmentSource, to_data_uri};
use crate::types::ToolName;

/// Top-level chat-completion request body.
#[derive(Debug, Serialize)]
pub(super) struct ChatRequestBody {
    pub model: String,
    pub messages: Vec<WireMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<ChatCompletionTools>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_completion_tokens: Option<u32>,
}

/// One message on the wire. `role` is the discriminant; only the variants we actually
/// emit are spelled out, so adding a new role (e.g. developer) is a deliberate choice.
#[derive(Debug, Serialize)]
#[serde(tag = "role", rename_all = "lowercase")]
pub(super) enum WireMessage {
    System {
        content: String,
    },
    User {
        content: UserMessageContent,
    },
    Assistant {
        #[serde(skip_serializing_if = "Option::is_none")]
        content: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        tool_calls: Option<Vec<ChatCompletionMessageToolCalls>>,
        /// DeepSeek V4 thinking-mode extension. When the prior assistant turn contained
        /// `tool_calls`, this field MUST be replayed verbatim on the next request or the
        /// API rejects with an `invalid_request_error`. Stock OpenAI tolerates the
        /// unknown field.
        #[serde(skip_serializing_if = "Option::is_none")]
        reasoning_content: Option<String>,
    },
    Tool {
        content: String,
        tool_call_id: String,
    },
}

/// A user message's `content`: either a plain string (text-only turn, the
/// common case) or an array of typed parts (when the turn carries images or
/// files). OpenAI accepts both shapes; we keep emitting the bare string when
/// there are no attachments so existing payloads are byte-for-byte unchanged.
#[derive(Debug, Serialize)]
#[serde(untagged)]
pub(super) enum UserMessageContent {
    Text(String),
    Parts(Vec<UserContentPart>),
}

/// One part of a multimodal user message. Shapes match the OpenAI Chat
/// Completions content-part schema (issue #187).
#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(super) enum UserContentPart {
    Text { text: String },
    ImageUrl { image_url: ImageUrlPart },
    File { file: FilePart },
}

#[derive(Debug, Serialize)]
pub(super) struct ImageUrlPart {
    pub url: String,
    pub detail: &'static str,
}

#[derive(Debug, Serialize)]
pub(super) struct FilePart {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filename: Option<String>,
    pub file_data: String,
}

/// Top-level chat-completion response body. We deserialize only the fields the agent
/// loop consumes so that future provider extensions don't break parsing.
#[derive(Debug, Deserialize)]
pub(super) struct ChatResponseBody {
    pub choices: Vec<WireChoice>,
    /// Model id reported by the provider — may differ from the request when a
    /// gateway routes to a specific snapshot. `default` because some
    /// OpenAI-compatible endpoints omit it.
    #[serde(default)]
    pub model: Option<String>,
    /// Token-usage counts. `default` because some endpoints omit the field on
    /// streaming responses; we do not stream today, but the safety net is cheap.
    #[serde(default)]
    pub usage: Option<WireUsage>,
}

/// Token-usage counters returned by an OpenAI-Chat-Completions endpoint. Field
/// names match the wire shape; we map to the provider-agnostic
/// [`crate::provider::chat::Usage`] in [`super::client`].
///
/// Cache-hit accounting is *not* part of the base OpenAI schema — each provider
/// adds its own variant. The structural decision is: deserialize every known
/// shape as `Option<u32>` (so a missing field is just `None`) and centralize
/// the priority order inside [`Self::cache_read_tokens`]. Adding a new provider
/// is then one field on this struct plus one fallback in that method — no
/// caller code changes.
///
/// Known shapes (verified against vendor docs, May 2026):
/// - DeepSeek: `usage.prompt_cache_hit_tokens` (int, top level).
///   See api-docs.deepseek.com/guides/kv_cache.
/// - OpenAI:   `usage.prompt_tokens_details.cached_tokens` (int, nested).
///   See developers.openai.com/api/docs/guides/prompt-caching.
#[derive(Debug, Deserialize, Default)]
pub(super) struct WireUsage {
    #[serde(default)]
    pub prompt_tokens: u32,
    #[serde(default)]
    pub completion_tokens: u32,
    #[serde(default)]
    pub prompt_cache_hit_tokens: Option<u32>,
    #[serde(default)]
    pub prompt_tokens_details: Option<WirePromptTokensDetails>,
}

#[derive(Debug, Deserialize, Default)]
pub(super) struct WirePromptTokensDetails {
    #[serde(default)]
    pub cached_tokens: Option<u32>,
}

impl WireUsage {
    /// Cache-hit input tokens, normalized across provider variants. Returns
    /// `None` only when no known shape was populated — distinct from `Some(0)`,
    /// which means the provider reported a zero-token cache hit.
    ///
    /// To add a new provider:
    ///   1. Add the field to `WireUsage` (or a nested wire struct).
    ///   2. Append one `.or_else(|| …)` clause below.
    pub(super) fn cache_read_tokens(&self) -> Option<u32> {
        // DeepSeek (top-level) takes precedence over the nested OpenAI shape so
        // a DeepSeek response that somehow carried both wouldn't silently drop
        // the explicit field. Order otherwise doesn't matter: a real response
        // populates at most one variant.
        self.prompt_cache_hit_tokens.or_else(|| {
            self.prompt_tokens_details
                .as_ref()
                .and_then(|d| d.cached_tokens)
        })
    }

    /// Cache-creation tokens. No OpenAI-compatible provider exposes an
    /// explicit creation event today — caching is opportunistic on their side,
    /// unlike Anthropic's `cache_control` opt-in. Kept as a hook so the shape
    /// matches Anthropic and future providers that adopt explicit creation can
    /// slot in here.
    pub(super) fn cache_creation_tokens(&self) -> Option<u32> {
        None
    }
}

#[derive(Debug, Deserialize)]
pub(super) struct WireChoice {
    #[serde(default)]
    pub finish_reason: Option<FinishReason>,
    pub message: WireResponseMessage,
}

#[derive(Debug, Deserialize)]
pub(super) struct WireResponseMessage {
    #[serde(default)]
    pub content: Option<String>,
    #[serde(default)]
    pub reasoning_content: Option<String>,
    #[serde(default)]
    pub tool_calls: Option<Vec<ChatCompletionMessageToolCalls>>,
}

/// Build the leading `role=system` message from the request's system prompt.
pub(super) fn system_message(prompt: &str) -> WireMessage {
    WireMessage::System {
        content: prompt.to_string(),
    }
}

/// Translate one provider-agnostic message into one or more wire messages.
///
/// Async + fallible: user attachments may need byte materialization (PDF/Office
/// inlined as base64 `file` parts) and capability gating — content the target
/// model cannot accept yields [`ProviderError::UnsupportedContent`] before any
/// part is emitted. This is the DeepSeek guard: its capabilities are all
/// `false`, so any attachment on the shared OpenAI wire path is rejected here.
pub(super) async fn message_to_wire(
    msg: ChatMessage,
    caps: ModelCapabilities,
    model: &str,
    source: &dyn AttachmentSource,
) -> Result<Vec<WireMessage>, ProviderError> {
    match msg {
        ChatMessage::User(content) => user_to_wire(content, caps, model, source).await,
        ChatMessage::Assistant(content) => Ok(assistant_to_wire(content)),
    }
}

/// A user turn may carry text, attachments, *and* tool results. Text-only and
/// tool-result turns keep their original shape (a bare-string `User` message
/// per text block, a `Tool` message per result) so existing payloads are
/// unchanged. When the turn carries an image/file, text + attachments collapse
/// into a single `User` message whose `content` is a parts array (OpenAI
/// requires the array form for multimodal); tool results still split out.
async fn user_to_wire(
    blocks: Vec<UserContent>,
    caps: ModelCapabilities,
    model: &str,
    source: &dyn AttachmentSource,
) -> Result<Vec<WireMessage>, ProviderError> {
    let has_attachment = blocks
        .iter()
        .any(|b| matches!(b, UserContent::Image(_) | UserContent::File(_)));

    if !has_attachment {
        let mut out: Vec<WireMessage> = Vec::with_capacity(blocks.len());
        for block in blocks {
            match block {
                UserContent::Text(t) => out.push(WireMessage::User {
                    content: UserMessageContent::Text(t),
                }),
                UserContent::ToolResult(r) => out.push(tool_message(r)),
                // `has_attachment` is false, so these arms are unreachable; map
                // them defensively rather than `unreachable!` (CLAUDE.md §12).
                UserContent::Image(att) | UserContent::File(att) => {
                    out.push(WireMessage::User {
                        content: UserMessageContent::Parts(vec![
                            attachment_part(att, caps, model, source).await?,
                        ]),
                    });
                }
            }
        }
        return Ok(out);
    }

    let mut parts: Vec<UserContentPart> = Vec::new();
    let mut tool_msgs: Vec<WireMessage> = Vec::new();
    for block in blocks {
        match block {
            UserContent::Text(t) => parts.push(UserContentPart::Text { text: t }),
            UserContent::Image(att) | UserContent::File(att) => {
                parts.push(attachment_part(att, caps, model, source).await?);
            }
            UserContent::ToolResult(r) => tool_msgs.push(tool_message(r)),
        }
    }

    let mut out = Vec::with_capacity(1 + tool_msgs.len());
    if !parts.is_empty() {
        out.push(WireMessage::User {
            content: UserMessageContent::Parts(parts),
        });
    }
    out.extend(tool_msgs);
    Ok(out)
}

/// One `role=tool` message for a tool result. OpenAI has no `is_error` flag;
/// failures are surfaced as plain text in `content`, which the caller already
/// did, so the bool is a no-op here.
fn tool_message(r: ToolResult) -> WireMessage {
    WireMessage::Tool {
        content: r.output,
        tool_call_id: r.call_id.as_str().to_string(),
    }
}

/// Build the content part for an image or file attachment, gating on model
/// capability first. Images ride as a URL (`image_url`); PDF/Office are fetched
/// and inlined as base64 `file` parts (OpenAI `file` does not accept a URL).
async fn attachment_part(
    att: Attachment,
    caps: ModelCapabilities,
    model: &str,
    source: &dyn AttachmentSource,
) -> Result<UserContentPart, ProviderError> {
    if !caps.accepts(att.mime()) {
        return Err(ProviderError::UnsupportedContent {
            mime: att.mime().as_mime(),
            model: model.to_owned(),
        });
    }
    if att.mime().is_image() {
        return Ok(UserContentPart::ImageUrl {
            image_url: ImageUrlPart {
                url: att.url().as_str().to_owned(),
                detail: "auto",
            },
        });
    }
    let bytes = source
        .fetch(att.url())
        .await
        .map_err(|e| ProviderError::Attachment(e.to_string()))?;
    Ok(UserContentPart::File {
        file: FilePart {
            filename: Some(att.filename().as_str().to_owned()),
            file_data: to_data_uri(att.mime(), &bytes),
        },
    })
}

/// An assistant turn collapses into a single wire message that carries any combination of
/// `content` (concatenated text), `tool_calls`, and `reasoning_content`.
fn assistant_to_wire(blocks: Vec<AssistantContent>) -> Vec<WireMessage> {
    let mut text = String::new();
    let mut reasoning = String::new();
    let mut tool_calls: Vec<ChatCompletionMessageToolCalls> = Vec::new();
    for block in blocks {
        match block {
            AssistantContent::Text(t) => {
                if !text.is_empty() {
                    text.push('\n');
                }
                text.push_str(&t);
            }
            AssistantContent::Reasoning(r) => {
                if !reasoning.is_empty() {
                    reasoning.push('\n');
                }
                reasoning.push_str(&r);
            }
            AssistantContent::ToolCall(call) => {
                tool_calls.push(ChatCompletionMessageToolCalls::Function(
                    ChatCompletionMessageToolCall {
                        id: call.id.as_str().to_string(),
                        function: FunctionCall {
                            name: call.name.as_str().to_string(),
                            arguments: call.input.to_string(),
                        },
                    },
                ));
            }
        }
    }

    // `bool::then_some` is the one-liner equivalent of
    // `if cond { Some(val) } else { None }`. The string/vec is moved into the call in
    // either branch — same semantics as the if-else, fewer lines.
    let content = (!text.is_empty()).then_some(text);
    let tool_calls = (!tool_calls.is_empty()).then_some(tool_calls);
    let reasoning_content = (!reasoning.is_empty()).then_some(reasoning);

    // A provider requires an assistant message to carry `content` or `tool_calls`;
    // `reasoning_content` alone is rejected (`invalid_request_error: content or
    // tool_calls must be set`), and that invalid message wedges the session on every
    // replay. A reasoning-only turn carries nothing the provider can act on — and the
    // DeepSeek thinking-mode rule only requires reasoning replay *alongside tool_calls*
    // — so we drop it from the replayed history. The block stays persisted for audit.
    if content.is_none() && tool_calls.is_none() {
        return Vec::new();
    }

    vec![WireMessage::Assistant {
        content,
        tool_calls,
        reasoning_content,
    }]
}

/// Map a tool spec to OpenAI's function-tool envelope. The `parameters` field is the
/// JSON-schema body verbatim — the registry already validated it.
pub(super) fn tool_spec_to_wire(spec: &ToolSpec) -> ChatCompletionTools {
    ChatCompletionTools::Function(ChatCompletionTool {
        function: FunctionObject {
            name: spec.name.as_str().to_string(),
            description: Some(spec.description.to_string()),
            parameters: Some((*spec.input_schema).clone()),
            strict: None,
        },
    })
}

/// Lift one choice from a response into our content vec. Skips tool calls whose id or
/// name we cannot parse — that means the upstream sent something we wouldn't have
/// registered, and the agent loop terminates cleanly rather than looping on garbage.
pub(super) fn choice_to_content(choice: WireChoice) -> Vec<AssistantContent> {
    let mut out: Vec<AssistantContent> = Vec::new();
    let msg = choice.message;

    // Reasoning first so it sits in front of the visible text in the session — matches
    // the order the model "thought, then spoke" and keeps replay deterministic.
    if let Some(r) = msg.reasoning_content
        && !r.is_empty()
    {
        out.push(AssistantContent::Reasoning(r));
    }

    if let Some(text) = msg.content
        && !text.is_empty()
    {
        out.push(AssistantContent::Text(text));
    }

    if let Some(calls) = msg.tool_calls {
        for call in calls {
            // The Custom variant is for OpenAI's experimental free-form tools; we don't
            // emit them and don't replay them.
            let ChatCompletionMessageToolCalls::Function(fc) = call else {
                continue;
            };
            let Ok(id) = ToolCallId::try_from(fc.id.as_str()) else {
                continue;
            };
            let Ok(name) = ToolName::try_from(fc.function.name.as_str()) else {
                continue;
            };
            // Arguments come back as a JSON-encoded string. If parsing fails the model
            // produced malformed JSON — surface as `Null` so the tool's own schema
            // validation rejects it with a clear error.
            let input = serde_json::from_str(&fc.function.arguments).unwrap_or(Value::Null);
            out.push(AssistantContent::ToolCall(ToolCall { id, name, input }));
        }
    }

    out
}

pub(super) fn map_finish_reason(reason: Option<FinishReason>) -> StopReason {
    match reason {
        Some(FinishReason::Stop) | None => StopReason::EndTurn,
        Some(FinishReason::ToolCalls | FinishReason::FunctionCall) => StopReason::ToolUse,
        Some(FinishReason::Length) => StopReason::MaxTokens,
        Some(FinishReason::ContentFilter) => StopReason::Other("content_filter".to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::Model;
    use crate::provider::attachment::RawAttachment;
    use crate::provider::chat::{ToolCall, ToolCallId, ToolResult};
    use crate::provider::materialize::test_support::StubAttachmentSource;
    use crate::types::ToolName;
    use serde_json::json;

    const ASSET: &str = "https://assets.example/attachments/x.bin";

    fn tool_id(s: &str) -> ToolCallId {
        ToolCallId::try_from(s).expect("valid id")
    }

    fn caps_for(model: &str) -> ModelCapabilities {
        Model::try_from(model).expect("known").capabilities()
    }

    fn attachment(mime: &str, name: &str) -> Attachment {
        Attachment::try_from(RawAttachment {
            url: ASSET.to_owned(),
            mime: mime.to_owned(),
            filename: name.to_owned(),
            size: 8,
        })
        .expect("valid")
    }

    /// Run the converter for an OpenAI model with an empty stub source (no
    /// fetches expected). Panics on any fetch attempt via the stub miss.
    async fn wire_openai(msg: ChatMessage) -> Vec<WireMessage> {
        let src = StubAttachmentSource::new();
        message_to_wire(msg, caps_for("gpt-5.5"), "gpt-5.5", &src)
            .await
            .expect("ok")
    }

    #[tokio::test]
    async fn user_text_becomes_one_user_message() {
        let wire = wire_openai(ChatMessage::User(vec![UserContent::Text("hi".into())])).await;
        assert_eq!(wire.len(), 1);
        assert!(matches!(wire[0], WireMessage::User { .. }));
    }

    #[tokio::test]
    async fn user_tool_results_split_into_tool_messages() {
        let wire = wire_openai(ChatMessage::User(vec![
            UserContent::ToolResult(ToolResult {
                call_id: tool_id("c1"),
                output: "ok".into(),
                is_error: false,
            }),
            UserContent::ToolResult(ToolResult {
                call_id: tool_id("c2"),
                output: "boom".into(),
                is_error: true,
            }),
        ]))
        .await;
        assert_eq!(wire.len(), 2);
        for m in &wire {
            assert!(matches!(m, WireMessage::Tool { .. }));
        }
    }

    #[tokio::test]
    async fn assistant_text_and_tool_calls_collapse_to_one_message() {
        let wire = wire_openai(ChatMessage::Assistant(vec![
            AssistantContent::Text("calling".into()),
            AssistantContent::ToolCall(ToolCall {
                id: tool_id("tc1"),
                name: ToolName::try_from("search").expect("valid"),
                input: json!({"q": "rust"}),
            }),
        ]))
        .await;
        assert_eq!(wire.len(), 1);
        let WireMessage::Assistant {
            content,
            tool_calls,
            reasoning_content,
        } = &wire[0]
        else {
            panic!("expected assistant");
        };
        assert!(content.is_some());
        assert_eq!(tool_calls.as_ref().map(Vec::len), Some(1));
        assert!(reasoning_content.is_none());
    }

    #[tokio::test]
    async fn assistant_reasoning_replayed_alongside_tool_calls() {
        let wire = wire_openai(ChatMessage::Assistant(vec![
            AssistantContent::Reasoning("thinking step 1".into()),
            AssistantContent::ToolCall(ToolCall {
                id: tool_id("tc1"),
                name: ToolName::try_from("search").expect("valid"),
                input: json!({}),
            }),
        ]))
        .await;
        let WireMessage::Assistant {
            reasoning_content, ..
        } = &wire[0]
        else {
            panic!("expected assistant");
        };
        assert_eq!(reasoning_content.as_deref(), Some("thinking step 1"));
    }

    #[tokio::test]
    async fn assistant_reasoning_only_is_dropped_from_wire() {
        // A turn that produced only reasoning (the model stopped without emitting text
        // or a tool call) has no `content` and no `tool_calls`. Emitting it with only
        // `reasoning_content` set is rejected by the provider with
        // `invalid_request_error: content or tool_calls must be set`, which wedges the
        // session on every replay. Drop it from the wire instead.
        let wire = wire_openai(ChatMessage::Assistant(vec![AssistantContent::Reasoning(
            "secret".into(),
        )]))
        .await;
        assert!(wire.is_empty());
    }

    #[tokio::test]
    async fn image_becomes_image_url_part_without_fetch() {
        let wire = wire_openai(ChatMessage::User(vec![
            UserContent::Text("look".into()),
            UserContent::Image(attachment("image/png", "a.png")),
        ]))
        .await;
        assert_eq!(wire.len(), 1);
        let json = serde_json::to_value(&wire[0]).expect("ser");
        assert_eq!(json["role"], "user");
        let parts = json["content"].as_array().expect("parts array");
        assert_eq!(parts[0]["type"], "text");
        assert_eq!(parts[1]["type"], "image_url");
        assert_eq!(parts[1]["image_url"]["url"], ASSET);
    }

    #[tokio::test]
    async fn pdf_becomes_base64_file_part() {
        let src = StubAttachmentSource::new().with(ASSET, b"%PDF-1.7 body".to_vec());
        let wire = message_to_wire(
            ChatMessage::User(vec![UserContent::File(attachment(
                "application/pdf",
                "r.pdf",
            ))]),
            caps_for("gpt-5.5"),
            "gpt-5.5",
            &src,
        )
        .await
        .expect("ok");
        let json = serde_json::to_value(&wire[0]).expect("ser");
        let part = &json["content"][0];
        assert_eq!(part["type"], "file");
        assert_eq!(part["file"]["filename"], "r.pdf");
        assert!(
            part["file"]["file_data"]
                .as_str()
                .expect("data")
                .starts_with("data:application/pdf;base64,")
        );
    }

    #[tokio::test]
    async fn office_becomes_base64_file_part_natively() {
        // OpenAI takes Office files natively (server-side augmentation); we do
        // NOT extract text — just inline the bytes as a `file` part.
        let src = StubAttachmentSource::new().with(ASSET, b"PK\x03\x04 xlsx".to_vec());
        let mime = "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet";
        let wire = message_to_wire(
            ChatMessage::User(vec![UserContent::File(attachment(mime, "data.xlsx"))]),
            caps_for("gpt-5.5"),
            "gpt-5.5",
            &src,
        )
        .await
        .expect("ok");
        let json = serde_json::to_value(&wire[0]).expect("ser");
        assert_eq!(json["content"][0]["type"], "file");
    }

    #[tokio::test]
    async fn deepseek_rejects_any_attachment() {
        // The core DeepSeek guard (issue #187): text-only model on the shared
        // OpenAI wire path must never receive an image/file part.
        let src = StubAttachmentSource::new();
        let err = message_to_wire(
            ChatMessage::User(vec![UserContent::Image(attachment("image/png", "a.png"))]),
            caps_for("deepseek-v4-pro"),
            "deepseek-v4-pro",
            &src,
        )
        .await
        .expect_err("must reject");
        assert!(matches!(err, ProviderError::UnsupportedContent { .. }));
    }

    #[tokio::test]
    async fn deepseek_still_accepts_text() {
        let src = StubAttachmentSource::new();
        let wire = message_to_wire(
            ChatMessage::User(vec![UserContent::Text("hi".into())]),
            caps_for("deepseek-v4-pro"),
            "deepseek-v4-pro",
            &src,
        )
        .await
        .expect("text ok");
        assert!(matches!(wire[0], WireMessage::User { .. }));
    }

    #[test]
    fn wire_usage_resolves_deepseek_cache_hit_field() {
        let u: WireUsage = serde_json::from_value(json!({
            "prompt_tokens": 100,
            "completion_tokens": 20,
            "prompt_cache_hit_tokens": 64,
            "prompt_cache_miss_tokens": 36,
        }))
        .expect("parses");
        assert_eq!(u.cache_read_tokens(), Some(64));
        assert_eq!(u.cache_creation_tokens(), None);
    }

    #[test]
    fn wire_usage_resolves_openai_cached_tokens_field() {
        let u: WireUsage = serde_json::from_value(json!({
            "prompt_tokens": 100,
            "completion_tokens": 20,
            "prompt_tokens_details": { "cached_tokens": 48 },
        }))
        .expect("parses");
        assert_eq!(u.cache_read_tokens(), Some(48));
        assert_eq!(u.cache_creation_tokens(), None);
    }

    #[test]
    fn wire_usage_cache_read_is_none_when_no_variant_present() {
        let u: WireUsage = serde_json::from_value(json!({
            "prompt_tokens": 100,
            "completion_tokens": 20,
        }))
        .expect("parses");
        assert_eq!(u.cache_read_tokens(), None);
        assert_eq!(u.cache_creation_tokens(), None);
    }

    #[test]
    fn wire_usage_cache_read_distinguishes_zero_from_missing() {
        // OpenAI emits `cached_tokens: 0` on sub-1024-token prompts; that's a
        // valid "the cache was checked and missed everything" signal, not
        // absence. Must round-trip as Some(0), not None.
        let u: WireUsage = serde_json::from_value(json!({
            "prompt_tokens": 100,
            "completion_tokens": 20,
            "prompt_tokens_details": { "cached_tokens": 0 },
        }))
        .expect("parses");
        assert_eq!(u.cache_read_tokens(), Some(0));
    }

    #[test]
    fn wire_usage_cache_read_prefers_deepseek_top_level_when_both_present() {
        // Defensive: if a future gateway returned both shapes, prefer the
        // explicit top-level field over the nested one. Order in the resolver
        // is the only thing that pins this — test guards against accidental
        // reordering.
        let u: WireUsage = serde_json::from_value(json!({
            "prompt_tokens": 100,
            "completion_tokens": 20,
            "prompt_cache_hit_tokens": 64,
            "prompt_tokens_details": { "cached_tokens": 48 },
        }))
        .expect("parses");
        assert_eq!(u.cache_read_tokens(), Some(64));
    }

    #[test]
    fn assistant_with_no_content_serializes_without_optional_fields() {
        // Defensive: an assistant turn with neither content, tool_calls, nor reasoning
        // (shouldn't happen but worth pinning) serializes to `{"role":"assistant"}`,
        // not to a body with explicit nulls that some providers reject.
        let wire = WireMessage::Assistant {
            content: None,
            tool_calls: None,
            reasoning_content: None,
        };
        let json = serde_json::to_string(&wire).expect("serializes");
        assert_eq!(json, r#"{"role":"assistant"}"#);
    }
}
