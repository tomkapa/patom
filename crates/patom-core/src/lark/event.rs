//! Inbound Lark event envelope (schema 2.0) parsing.
//!
//! The long-connection delivers each event as a `pbbp2` frame whose payload is
//! the JSON envelope `{schema, header, event}`. This module decodes the
//! envelope into a typed [`LarkEvent`] the bridge routes on. Only the events
//! the live path needs are modelled; everything else is [`LarkEvent::Other`].

use serde::Deserialize;

use crate::provider::limits::MAX_ATTACHMENTS_PER_MESSAGE;

use super::error::LarkError;
use super::mention::AtToken;
use super::resource::LarkResourceKind;
use super::types::{
    LarkAppId, LarkChatId, LarkEventId, LarkMessageId, LarkOpenId, LarkThreadId, LarkUserId,
    TenantKey,
};

/// Upper bound on rich-text (`post`) elements scanned for text + embedded
/// images. A `post` can nest many runs; the cap bounds the walk (CLAUDE.md §5)
/// — well above any human-authored message, so it only ever truncates a
/// pathological payload.
const LARK_POST_MAX_ELEMENTS: usize = 4096;

/// A decoded inbound event, narrowed to the live-path cases.
#[derive(Debug, Clone)]
pub enum LarkEvent {
    /// A chat message (`im.message.receive_v1`).
    Message(Box<InboundMessage>),
    /// The bot was added to a chat (`im.chat.member.bot.added_v1`) — triggers a
    /// roster sync.
    BotAdded(ChatMemberEvent),
    /// A user joined a chat (`im.chat.member.user.added_v1`) — roster refresh.
    UserAdded(ChatMemberEvent),
    /// A user left a chat (`im.chat.member.user.deleted_v1`).
    UserRemoved(ChatMemberEvent),
    /// An event we don't act on (card actions, other subscriptions).
    Other,
}

/// A normalized inbound chat message.
#[derive(Debug, Clone)]
pub struct InboundMessage {
    pub event_id: LarkEventId,
    pub app_id: LarkAppId,
    pub tenant_key: TenantKey,
    pub sender_open_id: LarkOpenId,
    /// `None` when the app lacks the contact scope or the sender is the bot.
    pub sender_user_id: Option<LarkUserId>,
    /// `"user"` or `"app"` (the bot's own messages).
    pub sender_type: String,
    pub chat_id: LarkChatId,
    /// `"group"` or `"p2p"` (DM).
    pub chat_type: String,
    pub message_id: LarkMessageId,
    /// The reply-thread anchor, when the message is in a topic/thread.
    pub thread_id: Option<LarkThreadId>,
    /// The extracted text body (empty for image/file; the flattened runs for a
    /// rich-text `post`).
    pub text: String,
    /// Image/file resources attached to the message (issue #187): standalone
    /// image/file messages and images embedded in a `post`. The bridge
    /// downloads + re-hosts the supported ones as model input.
    pub resources: Vec<LarkResource>,
    pub mentions: Vec<AtToken>,
}

/// A downloadable resource referenced by an inbound message.
#[derive(Debug, Clone)]
pub struct LarkResource {
    /// The `image_key` (images) or `file_key` (files) — the path param to the
    /// resource-download endpoint.
    pub file_key: String,
    pub kind: LarkResourceKind,
    /// The original filename: file messages carry `file_name`; `None` for
    /// images (the bridge synthesizes one from the downloaded content type).
    pub filename: Option<String>,
}

/// A chat-membership event, reduced to what a roster sync needs.
#[derive(Debug, Clone)]
pub struct ChatMemberEvent {
    pub event_id: LarkEventId,
    pub app_id: LarkAppId,
    pub tenant_key: TenantKey,
    pub chat_id: LarkChatId,
}

/// Parse a frame payload (the schema-2.0 envelope) into a [`LarkEvent`].
pub fn parse_event(payload: &[u8]) -> Result<LarkEvent, LarkError> {
    let env: Envelope = serde_json::from_slice(payload)?;
    let header = env.header;
    match header.event_type.as_str() {
        "im.message.receive_v1" => {
            let ev = env
                .event
                .ok_or_else(|| LarkError::Internal("message event missing body".to_owned()))?;
            parse_message(&header, &ev).map(|m| LarkEvent::Message(Box::new(m)))
        }
        "im.chat.member.bot.added_v1" => {
            parse_member(&header, env.event.as_ref()).map(LarkEvent::BotAdded)
        }
        "im.chat.member.user.added_v1" => {
            parse_member(&header, env.event.as_ref()).map(LarkEvent::UserAdded)
        }
        "im.chat.member.user.deleted_v1" => {
            parse_member(&header, env.event.as_ref()).map(LarkEvent::UserRemoved)
        }
        _ => Ok(LarkEvent::Other),
    }
}

fn parse_message(header: &Header, ev: &Event) -> Result<InboundMessage, LarkError> {
    let sender = ev
        .sender
        .as_ref()
        .ok_or_else(|| LarkError::Internal("message event missing sender".to_owned()))?;
    let message = ev
        .message
        .as_ref()
        .ok_or_else(|| LarkError::Internal("message event missing message".to_owned()))?;
    let sender_open_id = opt_id(sender.sender_id.open_id.as_deref())
        .ok_or_else(|| LarkError::Internal("message sender missing open_id".to_owned()))?;
    let sender_user_id = opt_id(sender.sender_id.user_id.as_deref())
        .map(LarkUserId::try_from)
        .transpose()?;
    let (text, resources) =
        parse_content(message.message_type.as_deref(), message.content.as_deref());
    let mentions = message
        .mentions
        .iter()
        .map(|m| AtToken {
            key: m.key.clone(),
            open_id: opt_string(m.id.open_id.as_deref()),
            name: m.name.clone(),
        })
        .collect();
    Ok(InboundMessage {
        event_id: LarkEventId::try_from(header.event_id.as_str())?,
        app_id: LarkAppId::try_from(header.app_id.as_str())?,
        tenant_key: TenantKey::try_from(header.tenant_key.as_str())?,
        sender_open_id: LarkOpenId::try_from(sender_open_id.as_str())?,
        sender_user_id,
        sender_type: sender.sender_type.clone().unwrap_or_default(),
        chat_id: LarkChatId::try_from(
            message
                .chat_id
                .as_deref()
                .ok_or_else(|| LarkError::Internal("message missing chat_id".to_owned()))?,
        )?,
        chat_type: message.chat_type.clone().unwrap_or_default(),
        message_id: LarkMessageId::try_from(
            message
                .message_id
                .as_deref()
                .ok_or_else(|| LarkError::Internal("message missing message_id".to_owned()))?,
        )?,
        thread_id: opt_id(message.thread_id.as_deref())
            .map(|s| LarkThreadId::try_from(s.as_str()))
            .transpose()?,
        text,
        resources,
        mentions,
    })
}

/// Extract the body text and any downloadable resources from a message's
/// `message_type` + `content` (a JSON string). Only the model-consumable kinds
/// are handled — `text`, `image`, `file`, and rich-text `post` (its runs are
/// flattened to text and its embedded images captured). Audio/video and
/// anything else yield no resources.
fn parse_content(message_type: Option<&str>, content: Option<&str>) -> (String, Vec<LarkResource>) {
    let Some(mt) = message_type else {
        return (String::new(), Vec::new());
    };
    let raw = content.unwrap_or_default();
    match mt {
        "text" => (extract_text(content).unwrap_or_default(), Vec::new()),
        "image" => {
            let resources = parse_image_key(raw)
                .map(|file_key| {
                    vec![LarkResource {
                        file_key,
                        kind: LarkResourceKind::Image,
                        filename: None,
                    }]
                })
                .unwrap_or_default();
            (String::new(), resources)
        }
        "file" => {
            let resources = parse_file(raw)
                .map(|(file_key, filename)| {
                    vec![LarkResource {
                        file_key,
                        kind: LarkResourceKind::File,
                        filename,
                    }]
                })
                .unwrap_or_default();
            (String::new(), resources)
        }
        "post" => parse_post(raw),
        _ => (String::new(), Vec::new()),
    }
}

/// `image_key` from an `image` message's content (`{"image_key":"img_v3_…"}`).
fn parse_image_key(raw: &str) -> Option<String> {
    let parsed: ContentImage = serde_json::from_str(raw).ok()?;
    parsed.image_key.filter(|k| !k.is_empty())
}

/// `(file_key, file_name)` from a `file` message's content
/// (`{"file_key":"file_v3_…","file_name":"report.pdf"}`).
fn parse_file(raw: &str) -> Option<(String, Option<String>)> {
    let parsed: ContentFile = serde_json::from_str(raw).ok()?;
    let key = parsed.file_key.filter(|k| !k.is_empty())?;
    let name = parsed.file_name.filter(|n| !n.is_empty());
    Some((key, name))
}

/// Flatten a `post` message: concatenate its text runs (one newline per row)
/// and capture embedded `img` keys as image resources. Bounded by
/// [`LARK_POST_MAX_ELEMENTS`] and [`MAX_ATTACHMENTS_PER_MESSAGE`] (§5); a larger
/// post is truncated, not rejected.
fn parse_post(raw: &str) -> (String, Vec<LarkResource>) {
    let Ok(post) = serde_json::from_str::<ContentPost>(raw) else {
        return (String::new(), Vec::new());
    };
    let mut text = String::new();
    let mut resources = Vec::new();
    let mut processed = 0usize;
    for row in &post.content {
        for el in row {
            // Graceful cap (operating input, not a programmer error): stop the
            // walk rather than crash on a pathologically large post.
            if processed >= LARK_POST_MAX_ELEMENTS {
                return (text.trim().to_owned(), resources);
            }
            processed += 1;
            match el.tag.as_str() {
                "text" | "a" => {
                    if let Some(t) = &el.text {
                        text.push_str(t);
                    }
                }
                "img" => {
                    if resources.len() < MAX_ATTACHMENTS_PER_MESSAGE
                        && let Some(key) = el.image_key.as_ref().filter(|k| !k.is_empty())
                    {
                        resources.push(LarkResource {
                            file_key: key.clone(),
                            kind: LarkResourceKind::Image,
                            filename: None,
                        });
                    }
                }
                _ => {}
            }
        }
        text.push('\n');
    }
    (text.trim().to_owned(), resources)
}

fn parse_member(header: &Header, event: Option<&Event>) -> Result<ChatMemberEvent, LarkError> {
    let chat_id = event
        .and_then(|e| e.chat_id.as_deref())
        .ok_or_else(|| LarkError::Internal("member event missing chat_id".to_owned()))?;
    Ok(ChatMemberEvent {
        event_id: LarkEventId::try_from(header.event_id.as_str())?,
        app_id: LarkAppId::try_from(header.app_id.as_str())?,
        tenant_key: TenantKey::try_from(header.tenant_key.as_str())?,
        chat_id: LarkChatId::try_from(chat_id)?,
    })
}

/// Extract `text` from a Lark text message's `content` (a JSON string).
fn extract_text(content: Option<&str>) -> Option<String> {
    let raw = content?;
    let parsed: ContentText = serde_json::from_str(raw).ok()?;
    parsed.text
}

/// `Some(trimmed)` iff the id string is present and non-empty.
fn opt_id(v: Option<&str>) -> Option<String> {
    v.map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

fn opt_string(v: Option<&str>) -> Option<String> {
    opt_id(v)
}

// ── Wire envelope (schema 2.0) ──────────────────────────────────────────────

#[derive(Deserialize)]
struct Envelope {
    header: Header,
    #[serde(default)]
    event: Option<Event>,
}

#[derive(Deserialize)]
struct Header {
    #[serde(default)]
    event_id: String,
    #[serde(default)]
    event_type: String,
    #[serde(default)]
    tenant_key: String,
    #[serde(default)]
    app_id: String,
}

#[derive(Deserialize)]
struct Event {
    #[serde(default)]
    sender: Option<Sender>,
    #[serde(default)]
    message: Option<Message>,
    #[serde(default)]
    chat_id: Option<String>,
}

// Field names mirror Lark's wire JSON verbatim (`sender_id`, `sender_type`).
#[derive(Deserialize)]
#[allow(clippy::struct_field_names)]
struct Sender {
    sender_id: SenderId,
    #[serde(default)]
    sender_type: Option<String>,
}

#[derive(Deserialize, Default)]
struct SenderId {
    #[serde(default)]
    open_id: Option<String>,
    #[serde(default)]
    user_id: Option<String>,
}

// Field names mirror Lark's wire JSON verbatim (`message_id`, `message_type`).
#[derive(Deserialize)]
#[allow(clippy::struct_field_names)]
struct Message {
    #[serde(default)]
    message_id: Option<String>,
    #[serde(default)]
    chat_id: Option<String>,
    #[serde(default)]
    chat_type: Option<String>,
    #[serde(default)]
    thread_id: Option<String>,
    #[serde(default)]
    message_type: Option<String>,
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    mentions: Vec<Mention>,
}

#[derive(Deserialize)]
struct Mention {
    #[serde(default)]
    key: String,
    #[serde(default)]
    id: MentionId,
    /// The mentioned party's display name (e.g. "Test User").
    #[serde(default)]
    name: String,
}

#[derive(Deserialize, Default)]
struct MentionId {
    #[serde(default)]
    open_id: Option<String>,
}

#[derive(Deserialize)]
struct ContentText {
    #[serde(default)]
    text: Option<String>,
}

#[derive(Deserialize)]
struct ContentImage {
    #[serde(default)]
    image_key: Option<String>,
}

#[derive(Deserialize)]
struct ContentFile {
    #[serde(default)]
    file_key: Option<String>,
    #[serde(default)]
    file_name: Option<String>,
}

/// A rich-text `post` body: rows of inline elements. Field names mirror Lark's
/// wire JSON verbatim.
#[derive(Deserialize)]
struct ContentPost {
    #[serde(default)]
    content: Vec<Vec<PostElement>>,
}

#[derive(Deserialize)]
struct PostElement {
    #[serde(default)]
    tag: String,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    image_key: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_group_text_message_with_mention() {
        let payload = br#"{
          "schema":"2.0",
          "header":{"event_id":"evt1","event_type":"im.message.receive_v1","tenant_key":"tk1","app_id":"cli_app"},
          "event":{
            "sender":{"sender_id":{"open_id":"ou_alice","user_id":"u_alice"},"sender_type":"user"},
            "message":{
              "message_id":"om_1","chat_id":"oc_chat","chat_type":"group","message_type":"text",
              "content":"{\"text\":\"@_user_1 draft a JD\"}",
              "mentions":[{"key":"@_user_1","id":{"open_id":"ou_bot"},"name":"Recruiter"}]
            }
          }
        }"#;
        let ev = parse_event(payload).expect("parse");
        let LarkEvent::Message(m) = ev else {
            panic!("expected message");
        };
        assert_eq!(m.app_id.as_str(), "cli_app");
        assert_eq!(m.tenant_key.as_str(), "tk1");
        assert_eq!(m.sender_open_id.as_str(), "ou_alice");
        assert_eq!(
            m.sender_user_id.as_ref().map(LarkUserId::as_str),
            Some("u_alice")
        );
        assert_eq!(m.chat_id.as_str(), "oc_chat");
        assert_eq!(m.chat_type, "group");
        assert_eq!(m.text, "@_user_1 draft a JD");
        assert_eq!(m.mentions.len(), 1);
        assert_eq!(m.mentions[0].open_id.as_deref(), Some("ou_bot"));
    }

    #[test]
    fn missing_user_id_is_none() {
        let payload = br#"{
          "header":{"event_id":"e","event_type":"im.message.receive_v1","tenant_key":"tk","app_id":"cli"},
          "event":{"sender":{"sender_id":{"open_id":"ou_x","user_id":""},"sender_type":"user"},
            "message":{"message_id":"om","chat_id":"oc","chat_type":"p2p","message_type":"text","content":"{\"text\":\"hi\"}"}}
        }"#;
        let LarkEvent::Message(m) = parse_event(payload).expect("parse") else {
            panic!("message");
        };
        assert!(m.sender_user_id.is_none());
        assert_eq!(m.text, "hi");
    }

    #[test]
    fn parses_bot_added_event() {
        let payload = br#"{
          "header":{"event_id":"e2","event_type":"im.chat.member.bot.added_v1","tenant_key":"tk","app_id":"cli"},
          "event":{"chat_id":"oc_added"}
        }"#;
        let ev = parse_event(payload).expect("parse");
        let LarkEvent::BotAdded(m) = ev else {
            panic!("expected bot added");
        };
        assert_eq!(m.chat_id.as_str(), "oc_added");
    }

    #[test]
    fn unknown_event_is_other() {
        let payload = br#"{"header":{"event_type":"im.chat.disbanded_v1","event_id":"e","tenant_key":"t","app_id":"c"}}"#;
        assert!(matches!(
            parse_event(payload).expect("parse"),
            LarkEvent::Other
        ));
    }

    fn message_payload(message_type: &str, content_json: &str) -> Vec<u8> {
        // `content_json` is the inner content object; embed it as a JSON string.
        let content_escaped = serde_json::to_string(content_json).expect("escape");
        format!(
            r#"{{"header":{{"event_id":"e","event_type":"im.message.receive_v1","tenant_key":"tk","app_id":"cli"}},
               "event":{{"sender":{{"sender_id":{{"open_id":"ou_x","user_id":"u_x"}},"sender_type":"user"}},
                 "message":{{"message_id":"om_1","chat_id":"oc","chat_type":"p2p","message_type":"{message_type}","content":{content_escaped}}}}}}}"#
        )
        .into_bytes()
    }

    #[test]
    fn parses_image_message_into_resource() {
        let payload = message_payload("image", r#"{"image_key":"img_v3_abc"}"#);
        let LarkEvent::Message(m) = parse_event(&payload).expect("parse") else {
            panic!("message");
        };
        assert_eq!(m.text, "");
        assert_eq!(m.resources.len(), 1);
        assert_eq!(m.resources[0].file_key, "img_v3_abc");
        assert_eq!(m.resources[0].kind, LarkResourceKind::Image);
        assert!(m.resources[0].filename.is_none());
    }

    #[test]
    fn parses_file_message_into_resource_with_name() {
        let payload = message_payload(
            "file",
            r#"{"file_key":"file_v3_xyz","file_name":"report.pdf"}"#,
        );
        let LarkEvent::Message(m) = parse_event(&payload).expect("parse") else {
            panic!("message");
        };
        assert_eq!(m.resources.len(), 1);
        assert_eq!(m.resources[0].file_key, "file_v3_xyz");
        assert_eq!(m.resources[0].kind, LarkResourceKind::File);
        assert_eq!(m.resources[0].filename.as_deref(), Some("report.pdf"));
    }

    #[test]
    fn parses_post_flattening_text_and_capturing_images() {
        let content = r#"{"title":"Update","content":[
            [{"tag":"text","text":"Please review "},{"tag":"a","text":"the doc","href":"http://x"}],
            [{"tag":"img","image_key":"img_v3_p1"}]
        ]}"#;
        let payload = message_payload("post", content);
        let LarkEvent::Message(m) = parse_event(&payload).expect("parse") else {
            panic!("message");
        };
        assert!(m.text.contains("Please review the doc"), "got {:?}", m.text);
        assert_eq!(m.resources.len(), 1);
        assert_eq!(m.resources[0].file_key, "img_v3_p1");
        assert_eq!(m.resources[0].kind, LarkResourceKind::Image);
    }

    #[test]
    fn audio_message_yields_no_resources() {
        let payload = message_payload("audio", r#"{"file_key":"file_v3_a","duration":1200}"#);
        let LarkEvent::Message(m) = parse_event(&payload).expect("parse") else {
            panic!("message");
        };
        assert!(m.text.is_empty());
        assert!(m.resources.is_empty(), "audio is not a model input");
    }
}
