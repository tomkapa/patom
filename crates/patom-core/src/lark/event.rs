//! Inbound Lark event envelope (schema 2.0) parsing.
//!
//! The long-connection delivers each event as a `pbbp2` frame whose payload is
//! the JSON envelope `{schema, header, event}`. This module decodes the
//! envelope into a typed [`LarkEvent`] the bridge routes on. Only the events
//! the live path needs are modelled; everything else is [`LarkEvent::Other`].

use serde::Deserialize;

use super::error::LarkError;
use super::mention::AtToken;
use super::types::{
    LarkAppId, LarkChatId, LarkEventId, LarkMessageId, LarkOpenId, LarkThreadId, LarkUserId,
    TenantKey,
};

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
    /// The extracted text body (empty for non-text message types).
    pub text: String,
    pub mentions: Vec<AtToken>,
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
    let text = message
        .message_type
        .as_deref()
        .filter(|t| *t == "text")
        .and_then(|_| extract_text(message.content.as_deref()))
        .unwrap_or_default();
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
        mentions,
    })
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
}
