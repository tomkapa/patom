//! Gateway dispatch-payload parsing (serde at the boundary, §1).
//!
//! The connection loop hands the bridge a raw `(event_type, data)`; this module
//! turns the handful of events Patom acts on — `MESSAGE_CREATE`, `GUILD_CREATE`,
//! `GUILD_MEMBER_ADD`/`UPDATE` — into typed structs, and ignores the rest. Every
//! id funnels through its snowflake newtype's `Deserialize` (a JSON string, never
//! a number), so a corrupt id is rejected here rather than corrupting state.

use serde::Deserialize;

use crate::types::ParseError;

use super::error::DiscordError;
use super::limits::DISCORD_CUSTOM_ID_MAX;
use super::types::{
    ApplicationId, ContainerId, DiscordMessageId, DiscordUserId, GuildId, InteractionId,
    InteractionToken,
};

/// A normalized inbound Gateway event (only the kinds Patom acts on).
#[derive(Debug, Clone)]
pub enum DiscordEvent {
    Message(Box<InboundMessage>),
    GuildCreate(Box<GuildCreate>),
    /// `GUILD_MEMBER_ADD` / `GUILD_MEMBER_UPDATE` — refresh a single member.
    MemberUpsert(Box<GuildMemberEvent>),
    /// `INTERACTION_CREATE` — a component interaction (e.g. an approval button
    /// click). Arrives over the authenticated Gateway, so it carries no HMAC.
    Interaction(Box<InboundInteraction>),
    Other,
}

/// A Discord user object (the `author` of a message, or a member's `user`).
#[derive(Debug, Clone, Deserialize)]
pub struct Author {
    pub id: DiscordUserId,
    #[serde(default)]
    pub username: String,
    #[serde(default)]
    pub global_name: Option<String>,
    /// `true` for a bot/app author (filtered out of the human-shadow path).
    #[serde(default)]
    pub bot: bool,
}

impl Author {
    /// The display name to show the agent: `nick > global_name > username`.
    #[must_use]
    pub fn display_name(&self, nick: Option<&str>) -> Option<String> {
        nick.map(str::to_owned)
            .or_else(|| self.global_name.clone())
            .or_else(|| (!self.username.is_empty()).then(|| self.username.clone()))
    }
}

#[derive(Debug, Clone, Deserialize)]
struct Member {
    #[serde(default)]
    nick: Option<String>,
}

/// A `MESSAGE_CREATE` (or the same shape from a history backfill).
#[derive(Debug, Clone, Deserialize)]
#[serde(from = "RawMessage")]
pub struct InboundMessage {
    pub message_id: DiscordMessageId,
    /// The container the message lives in (a top-level channel or a thread).
    pub channel_id: ContainerId,
    /// Absent for a DM (no guild).
    pub guild_id: Option<GuildId>,
    pub author: Author,
    /// The per-guild nickname, when present.
    pub member_nick: Option<String>,
    /// Empty without the `MESSAGE_CONTENT` intent (except DMs / bot-own / @-mentions).
    pub content: String,
    /// The full user objects explicitly `@`-mentioned (id + name, for rendering
    /// `<@id>` → `@Name`).
    pub mentions: Vec<Author>,
    /// Uploaded files/images on the message (issue #187). Each carries a signed
    /// CDN `url` valid at receipt; the bridge downloads + re-hosts the supported
    /// ones as model input.
    pub attachments: Vec<DiscordAttachment>,
    /// Present (the webhook's id) when the message was webhook-authored.
    pub webhook_id: Option<String>,
}

/// One Discord message attachment, validated at the parse boundary
/// (CLAUDE.md §1) — `filename` and `url` are guaranteed non-empty.
///
/// `content_type` is optional — Discord omits it for some uploads — so the
/// bridge falls back to the filename extension. `url` is a signed
/// `cdn.discordapp.com` link that needs no auth and is valid when the Gateway
/// delivers the message.
#[derive(Debug, Clone)]
pub struct DiscordAttachment {
    pub filename: String,
    pub content_type: Option<String>,
    pub size: u64,
    pub url: String,
}

/// Wire shape of a Discord attachment object. Funnels into [`DiscordAttachment`]
/// via [`TryFrom`]; a malformed entry is dropped (not message-fatal) in
/// [`From<RawMessage>`], so one bad attachment never costs the whole message.
#[derive(Deserialize)]
struct RawDiscordAttachment {
    #[serde(default)]
    filename: String,
    #[serde(default)]
    content_type: Option<String>,
    #[serde(default)]
    size: u64,
    #[serde(default)]
    url: String,
}

impl TryFrom<RawDiscordAttachment> for DiscordAttachment {
    type Error = ParseError;

    fn try_from(raw: RawDiscordAttachment) -> Result<Self, Self::Error> {
        if raw.filename.is_empty() {
            return Err(ParseError::Empty {
                field: "discord.attachment.filename",
            });
        }
        if raw.url.is_empty() {
            return Err(ParseError::Empty {
                field: "discord.attachment.url",
            });
        }
        Ok(Self {
            filename: raw.filename,
            content_type: raw.content_type,
            size: raw.size,
            url: raw.url,
        })
    }
}

/// The wire shape of a message, before flattening into [`InboundMessage`].
#[derive(Deserialize)]
struct RawMessage {
    id: DiscordMessageId,
    channel_id: ContainerId,
    #[serde(default)]
    guild_id: Option<GuildId>,
    author: Author,
    #[serde(default)]
    member: Option<Member>,
    #[serde(default)]
    content: String,
    #[serde(default)]
    mentions: Vec<Author>,
    #[serde(default)]
    attachments: Vec<RawDiscordAttachment>,
    #[serde(default)]
    webhook_id: Option<String>,
}

impl From<RawMessage> for InboundMessage {
    fn from(raw: RawMessage) -> Self {
        Self {
            message_id: raw.id,
            channel_id: raw.channel_id,
            guild_id: raw.guild_id,
            author: raw.author,
            member_nick: raw.member.and_then(|m| m.nick),
            content: raw.content,
            mentions: raw.mentions,
            // Drop malformed attachments (missing filename/url) at the boundary;
            // a bad entry must not fail the whole message.
            attachments: raw
                .attachments
                .into_iter()
                .filter_map(|a| DiscordAttachment::try_from(a).ok())
                .collect(),
            webhook_id: raw.webhook_id,
        }
    }
}

impl InboundMessage {
    /// Whether the message `@`-mentions the given bot user.
    ///
    /// Primary signal is the `mentions` array. As a fallback we also scan the raw
    /// `content` for the bot's mention marker (`<@id>` / `<@!id>`): a top-level
    /// channel `@mention` is occasionally delivered with an empty `mentions`
    /// array, and Discord grants `MESSAGE_CONTENT` for a message that mentions the
    /// bot, so the marker is present whenever the bot is genuinely addressed.
    #[must_use]
    pub fn mentions_bot(&self, bot_user_id: &DiscordUserId) -> bool {
        if self.mentions.iter().any(|m| m.id == *bot_user_id) {
            return true;
        }
        let id = bot_user_id.as_str();
        let plain = format!("<@{id}>");
        let nick = format!("<@!{id}>");
        self.content.contains(&plain) || self.content.contains(&nick)
    }

    /// `(id, display_name)` pairs for the mentioned users, for rewriting `<@id>`
    /// markers to readable `@Name` (drops a mention with no resolvable name).
    #[must_use]
    pub fn mention_names(&self) -> Vec<(DiscordUserId, String)> {
        self.mentions
            .iter()
            .filter_map(|a| a.display_name(None).map(|n| (a.id.clone(), n)))
            .collect()
    }
}

/// An `INTERACTION_CREATE` for a message component (Discord type 3) — e.g. a
/// click on an approval Approve/Deny button.
///
/// The clicker is `member.user` in a guild and the top-level `user` in a DM;
/// [`From<RawInteraction>`] folds both into `user`. The `custom_id` is the
/// button's echoed id (`apv:{approval_id}:{a|d}`); it is dropped (set `None`)
/// when it exceeds Discord's [`DISCORD_CUSTOM_ID_MAX`] cap so a malformed frame
/// never carries an oversized value downstream.
#[derive(Debug, Clone, Deserialize)]
#[serde(from = "RawInteraction")]
pub struct InboundInteraction {
    pub interaction_id: InteractionId,
    /// The app (bot) the interaction targets — the token-fetch + edit key.
    pub application_id: ApplicationId,
    /// The continuation token (callback / `@original` edit), valid ~15 min.
    pub interaction_token: InteractionToken,
    /// Discord interaction type (`3` = MESSAGE_COMPONENT). Others are ignored.
    pub interaction_type: u8,
    /// The channel the component message lives in, when present.
    pub channel_id: Option<ContainerId>,
    /// The clicking user (`member.user` in a guild, `user` in a DM).
    pub user: Option<Author>,
    /// The clicker's per-guild nickname, when present.
    pub nick: Option<String>,
    /// The clicked component's `custom_id` (e.g. `apv:{id}:a`), within cap.
    pub custom_id: Option<String>,
}

/// Discord's MESSAGE_COMPONENT interaction type code.
pub const DISCORD_INTERACTION_TYPE_COMPONENT: u8 = 3;

/// The wire shape of an interaction, before flattening into
/// [`InboundInteraction`].
#[derive(Deserialize)]
struct RawInteraction {
    id: InteractionId,
    application_id: ApplicationId,
    token: InteractionToken,
    #[serde(rename = "type")]
    interaction_type: u8,
    #[serde(default)]
    channel_id: Option<ContainerId>,
    #[serde(default)]
    member: Option<RawInteractionMember>,
    #[serde(default)]
    user: Option<Author>,
    #[serde(default)]
    data: Option<RawInteractionData>,
}

#[derive(Deserialize)]
struct RawInteractionMember {
    #[serde(default)]
    user: Option<Author>,
    #[serde(default)]
    nick: Option<String>,
}

#[derive(Deserialize)]
struct RawInteractionData {
    #[serde(default)]
    custom_id: Option<String>,
}

impl From<RawInteraction> for InboundInteraction {
    fn from(raw: RawInteraction) -> Self {
        let (member_user, nick) = match raw.member {
            Some(m) => (m.user, m.nick),
            None => (None, None),
        };
        // `member.user` (guild) wins; fall back to the top-level `user` (DM).
        let user = member_user.or(raw.user);
        let custom_id = raw
            .data
            .and_then(|d| d.custom_id)
            .filter(|c| c.chars().count() <= DISCORD_CUSTOM_ID_MAX);
        Self {
            interaction_id: raw.id,
            application_id: raw.application_id,
            interaction_token: raw.token,
            interaction_type: raw.interaction_type,
            channel_id: raw.channel_id,
            user,
            nick,
            custom_id,
        }
    }
}

/// A roster member (from `GUILD_CREATE.members` or a member event).
#[derive(Debug, Clone, Deserialize)]
pub struct RosterMember {
    pub user: Author,
    #[serde(default)]
    pub nick: Option<String>,
}

/// `GUILD_CREATE` — the connect-time guild snapshot, with an initial member page.
#[derive(Debug, Clone, Deserialize)]
pub struct GuildCreate {
    #[serde(rename = "id")]
    pub guild_id: GuildId,
    #[serde(default)]
    pub members: Vec<RosterMember>,
}

/// `GUILD_MEMBER_ADD` / `GUILD_MEMBER_UPDATE` — a single member upsert.
#[derive(Debug, Clone, Deserialize)]
pub struct GuildMemberEvent {
    pub guild_id: GuildId,
    pub user: Author,
    #[serde(default)]
    pub nick: Option<String>,
}

/// Parse a dispatch `(event_type, data)` into the normalized event. Unknown or
/// unhandled types map to [`DiscordEvent::Other`].
pub fn parse(event_type: &str, data: &serde_json::Value) -> Result<DiscordEvent, DiscordError> {
    match event_type {
        "MESSAGE_CREATE" => {
            let msg: InboundMessage = serde_json::from_value(data.clone())?;
            Ok(DiscordEvent::Message(Box::new(msg)))
        }
        "GUILD_CREATE" => {
            let gc: GuildCreate = serde_json::from_value(data.clone())?;
            Ok(DiscordEvent::GuildCreate(Box::new(gc)))
        }
        "GUILD_MEMBER_ADD" | "GUILD_MEMBER_UPDATE" => {
            let ev: GuildMemberEvent = serde_json::from_value(data.clone())?;
            Ok(DiscordEvent::MemberUpsert(Box::new(ev)))
        }
        "INTERACTION_CREATE" => {
            let intr: InboundInteraction = serde_json::from_value(data.clone())?;
            Ok(DiscordEvent::Interaction(Box::new(intr)))
        }
        _ => Ok(DiscordEvent::Other),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_message_create_extracts_fields() {
        let data = serde_json::json!({
            "id": "111111111111111111",
            "channel_id": "222222222222222222",
            "guild_id": "333333333333333333",
            "author": {"id": "444444444444444444", "username": "alice", "global_name": "Alice A"},
            "member": {"nick": "Ali"},
            "content": "@Recruiter draft a JD",
            "mentions": [{"id": "555555555555555555"}],
        });
        let DiscordEvent::Message(m) = parse("MESSAGE_CREATE", &data).expect("parse") else {
            panic!("expected a message");
        };
        assert_eq!(m.message_id.as_str(), "111111111111111111");
        assert_eq!(m.channel_id.as_str(), "222222222222222222");
        assert_eq!(
            m.guild_id.as_ref().map(GuildId::as_str),
            Some("333333333333333333")
        );
        assert!(!m.author.bot);
        // nick wins over global_name and username.
        assert_eq!(
            m.author.display_name(m.member_nick.as_deref()).as_deref(),
            Some("Ali")
        );
        let bot = DiscordUserId::try_from("555555555555555555").expect("bot");
        assert!(m.mentions_bot(&bot));
        assert!(m.webhook_id.is_none());
    }

    #[test]
    fn parse_message_create_captures_attachments() {
        let data = serde_json::json!({
            "id": "1", "channel_id": "2", "guild_id": "3",
            "author": {"id": "9", "username": "alice"},
            "content": "see attached",
            "attachments": [
                {
                    "id": "77", "filename": "report.pdf", "size": 1024,
                    "content_type": "application/pdf",
                    "url": "https://cdn.discordapp.com/attachments/2/77/report.pdf?ex=a&is=b&hm=c",
                    "proxy_url": "https://media.discordapp.net/attachments/2/77/report.pdf",
                },
                {
                    // content_type omitted — the bridge falls back to the extension.
                    "id": "78", "filename": "pic.png", "size": 2048,
                    "url": "https://cdn.discordapp.com/attachments/2/78/pic.png?ex=a&is=b&hm=c",
                },
            ],
        });
        let DiscordEvent::Message(m) = parse("MESSAGE_CREATE", &data).expect("parse") else {
            panic!("message");
        };
        assert_eq!(m.attachments.len(), 2);
        assert_eq!(m.attachments[0].filename, "report.pdf");
        assert_eq!(
            m.attachments[0].content_type.as_deref(),
            Some("application/pdf")
        );
        assert_eq!(m.attachments[0].size, 1024);
        assert!(
            m.attachments[0]
                .url
                .starts_with("https://cdn.discordapp.com/")
        );
        assert_eq!(m.attachments[1].filename, "pic.png");
        assert!(m.attachments[1].content_type.is_none());
    }

    #[test]
    fn parse_message_drops_malformed_attachment_but_keeps_the_message() {
        // One attachment is missing its url (malformed). It must be dropped at
        // the boundary while the valid one — and the message — survive.
        let data = serde_json::json!({
            "id": "1", "channel_id": "2", "guild_id": "3",
            "author": {"id": "9", "username": "alice"},
            "content": "two files",
            "attachments": [
                {"id": "1", "filename": "ok.png", "size": 64, "content_type": "image/png",
                 "url": "https://cdn.discordapp.com/attachments/2/1/ok.png"},
                {"id": "2", "filename": "broken.png", "size": 64},
            ],
        });
        let DiscordEvent::Message(m) = parse("MESSAGE_CREATE", &data).expect("parse") else {
            panic!("message");
        };
        assert_eq!(m.attachments.len(), 1, "malformed entry dropped");
        assert_eq!(m.attachments[0].filename, "ok.png");
    }

    #[test]
    fn parse_message_without_attachments_is_empty_vec() {
        let data = serde_json::json!({
            "id": "1", "channel_id": "2",
            "author": {"id": "3", "username": "carol"},
            "content": "hi",
        });
        let DiscordEvent::Message(m) = parse("MESSAGE_CREATE", &data).expect("parse") else {
            panic!("message");
        };
        assert!(m.attachments.is_empty());
    }

    #[test]
    fn mentions_bot_via_content_marker_when_array_empty() {
        // A top-level channel @mention can arrive with an empty `mentions` array
        // but the `<@id>` marker still in `content`. The trigger must still fire.
        let bot = DiscordUserId::try_from("555555555555555555").expect("bot");
        for marker in ["<@555555555555555555>", "<@!555555555555555555>"] {
            let data = serde_json::json!({
                "id": "1", "channel_id": "2", "guild_id": "3",
                "author": {"id": "9", "username": "alice"},
                "content": format!("hey {marker} draft a JD"),
                "mentions": [],
            });
            let DiscordEvent::Message(m) = parse("MESSAGE_CREATE", &data).expect("parse") else {
                panic!("message");
            };
            assert!(m.mentions.is_empty());
            assert!(m.mentions_bot(&bot), "content marker {marker} triggers");
        }
        // A different id in the content does not falsely trigger.
        let other = DiscordUserId::try_from("111111111111111111").expect("other");
        let data = serde_json::json!({
            "id": "1", "channel_id": "2", "guild_id": "3",
            "author": {"id": "9", "username": "alice"},
            "content": "hey <@555555555555555555> hi",
            "mentions": [],
        });
        let DiscordEvent::Message(m) = parse("MESSAGE_CREATE", &data).expect("parse") else {
            panic!("message");
        };
        assert!(!m.mentions_bot(&other));
    }

    #[test]
    fn display_name_falls_back_global_then_username() {
        let a = Author {
            id: DiscordUserId::try_from("1").expect("id"),
            username: "bob".to_owned(),
            global_name: Some("Bob B".to_owned()),
            bot: false,
        };
        assert_eq!(a.display_name(None).as_deref(), Some("Bob B"));
        let a2 = Author {
            global_name: None,
            ..a
        };
        assert_eq!(a2.display_name(None).as_deref(), Some("bob"));
    }

    #[test]
    fn dm_message_has_no_guild() {
        let data = serde_json::json!({
            "id": "1", "channel_id": "2",
            "author": {"id": "3", "username": "carol"},
            "content": "hi",
        });
        let DiscordEvent::Message(m) = parse("MESSAGE_CREATE", &data).expect("parse") else {
            panic!("message");
        };
        assert!(m.guild_id.is_none());
        assert!(m.mentions.is_empty());
    }

    #[test]
    fn webhook_message_carries_webhook_id() {
        let data = serde_json::json!({
            "id": "1", "channel_id": "2", "guild_id": "3",
            "author": {"id": "9", "username": "hook"},
            "content": "from a webhook",
            "webhook_id": "987654321098765432",
        });
        let DiscordEvent::Message(m) = parse("MESSAGE_CREATE", &data).expect("parse") else {
            panic!("message");
        };
        assert_eq!(m.webhook_id.as_deref(), Some("987654321098765432"));
    }

    #[test]
    fn parse_guild_create_reads_members() {
        let data = serde_json::json!({
            "id": "333333333333333333",
            "members": [
                {"user": {"id": "1", "username": "alice", "global_name": "Alice"}, "nick": "Ali"},
                {"user": {"id": "2", "username": "bot", "bot": true}},
            ],
        });
        let DiscordEvent::GuildCreate(gc) = parse("GUILD_CREATE", &data).expect("parse") else {
            panic!("guild create");
        };
        assert_eq!(gc.guild_id.as_str(), "333333333333333333");
        assert_eq!(gc.members.len(), 2);
        assert!(gc.members[1].user.bot);
    }

    #[test]
    fn parse_interaction_create_guild_button_click() {
        let data = serde_json::json!({
            "id": "111111111111111111",
            "application_id": "222222222222222222",
            "token": "aW50ZXJhY3Rpb24tdG9rZW4",
            "type": 3,
            "channel_id": "333333333333333333",
            "member": {
                "user": {"id": "444444444444444444", "username": "alice", "global_name": "Alice"},
                "nick": "Ali",
            },
            "data": {"custom_id": "apv:7e57c0de-0000-4000-8000-000000000001:a", "component_type": 2},
        });
        let DiscordEvent::Interaction(i) = parse("INTERACTION_CREATE", &data).expect("parse")
        else {
            panic!("expected an interaction");
        };
        assert_eq!(i.interaction_id.as_str(), "111111111111111111");
        assert_eq!(i.application_id.as_str(), "222222222222222222");
        assert_eq!(i.interaction_token.expose(), "aW50ZXJhY3Rpb24tdG9rZW4");
        assert_eq!(i.interaction_type, DISCORD_INTERACTION_TYPE_COMPONENT);
        assert_eq!(
            i.channel_id.as_ref().map(ContainerId::as_str),
            Some("333333333333333333")
        );
        // The clicker is `member.user` in a guild.
        assert_eq!(
            i.user.as_ref().map(|u| u.id.as_str()),
            Some("444444444444444444")
        );
        assert_eq!(i.nick.as_deref(), Some("Ali"));
        assert_eq!(
            i.custom_id.as_deref(),
            Some("apv:7e57c0de-0000-4000-8000-000000000001:a")
        );
    }

    #[test]
    fn parse_interaction_create_dm_uses_top_level_user() {
        // A DM interaction has no `member`; the clicker is the top-level `user`.
        let data = serde_json::json!({
            "id": "1", "application_id": "2", "token": "tok", "type": 3,
            "user": {"id": "999999999999999999", "username": "bob"},
            "data": {"custom_id": "apv:7e57c0de-0000-4000-8000-000000000002:d"},
        });
        let DiscordEvent::Interaction(i) = parse("INTERACTION_CREATE", &data).expect("parse")
        else {
            panic!("interaction");
        };
        assert_eq!(
            i.user.as_ref().map(|u| u.id.as_str()),
            Some("999999999999999999")
        );
        assert!(i.nick.is_none());
        assert_eq!(
            i.custom_id.as_deref(),
            Some("apv:7e57c0de-0000-4000-8000-000000000002:d")
        );
    }

    #[test]
    fn parse_interaction_drops_oversized_custom_id() {
        // A custom_id past Discord's 100-char cap is a malformed frame; it is
        // dropped (None) rather than carried downstream.
        let data = serde_json::json!({
            "id": "1", "application_id": "2", "token": "tok", "type": 3,
            "user": {"id": "3", "username": "x"},
            "data": {"custom_id": "a".repeat(101)},
        });
        let DiscordEvent::Interaction(i) = parse("INTERACTION_CREATE", &data).expect("parse")
        else {
            panic!("interaction");
        };
        assert!(i.custom_id.is_none());
    }

    #[test]
    fn parse_interaction_rejects_blank_token() {
        let data = serde_json::json!({
            "id": "1", "application_id": "2", "token": "", "type": 3,
            "user": {"id": "3", "username": "x"},
        });
        assert!(parse("INTERACTION_CREATE", &data).is_err());
    }

    #[test]
    fn unknown_event_is_other() {
        let data = serde_json::json!({});
        assert!(matches!(
            parse("TYPING_START", &data).expect("parse"),
            DiscordEvent::Other
        ));
    }

    #[test]
    fn malformed_snowflake_is_rejected() {
        let data = serde_json::json!({
            "id": "not-digits", "channel_id": "2",
            "author": {"id": "3", "username": "x"},
        });
        assert!(parse("MESSAGE_CREATE", &data).is_err());
    }
}
