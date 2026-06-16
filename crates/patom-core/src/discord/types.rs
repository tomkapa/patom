//! Domain newtypes for the Discord adapter (CLAUDE.md §1 — parse, don't
//! validate).
//!
//! Every value that crosses into the Discord module funnels through one of these
//! `TryFrom` constructors. The boundaries are the Gateway JSON codec
//! (`codec.rs` / `event.rs`), the REST responses (`poster.rs` / `history.rs`),
//! and the admin registration route (`admin_routes.rs`). Downstream code never
//! reconstructs IDs from raw strings.
//!
//! **Snowflakes are parsed from a JSON *string*, never a number.** Discord sends
//! every id as a JSON string precisely because a 64-bit id does not survive a
//! 53-bit JS `Number` / `f64` round-trip. The custom `Deserialize` below reads a
//! `String` and validates it is ASCII digits — there is no `f64` path that could
//! corrupt the low bits.

use std::fmt;
use std::sync::Arc;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::types::{ParseError, SecretString};

/// Maximum length (bytes) for a Discord snowflake id. A `u64` is at most 20
/// decimal digits (`18446744073709551615`); cap there so a hostile frame cannot
/// smuggle a multi-kilobyte string past the boundary.
pub const DISCORD_SNOWFLAKE_MAX_LEN: usize = 20;

/// Maximum length for a Discord bot token. Tokens are ~70 bytes
/// (`base64(app_id).base64(ts).hmac`); cap at 256 with head-room.
pub const DISCORD_TOKEN_MAX_LEN: usize = 256;

// ─────────────────────────────────────────────────────────────────────────
// Opaque identifiers (snowflakes)
// ─────────────────────────────────────────────────────────────────────────

/// Emit a `Clone + PartialEq + Eq + Hash` newtype around `Arc<str>` whose
/// `TryFrom<&str>` / `TryFrom<String>` funnel through [`parse_snowflake`]
/// (digits-only), plus the `Debug` / `Display` / `Serialize` / `Deserialize`
/// glue. The `Deserialize` reads a JSON **string** — never a number — so the id
/// never passes through `f64`.
macro_rules! discord_snowflake_newtype {
    (
        $(#[$meta:meta])*
        $name:ident, $field:literal
    ) => {
        $(#[$meta])*
        #[derive(Clone, PartialEq, Eq, Hash)]
        pub struct $name(Arc<str>);

        impl $name {
            #[must_use]
            pub fn as_str(&self) -> &str { &self.0 }
        }

        impl TryFrom<&str> for $name {
            type Error = ParseError;
            fn try_from(raw: &str) -> Result<Self, Self::Error> {
                parse_snowflake(raw, $field).map(|s| Self(Arc::from(s)))
            }
        }

        impl TryFrom<String> for $name {
            type Error = ParseError;
            fn try_from(raw: String) -> Result<Self, Self::Error> {
                Self::try_from(raw.as_str())
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.debug_tuple(stringify!($name)).field(&&*self.0).finish()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl Serialize for $name {
            fn serialize<S: Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
                ser.serialize_str(&self.0)
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
                let raw = String::deserialize(de)?;
                Self::try_from(raw).map_err(serde::de::Error::custom)
            }
        }
    };
}

discord_snowflake_newtype! {
    /// A Discord guild (server) snowflake. The org/tenant anchor.
    GuildId, "discord_guild_id"
}

discord_snowflake_newtype! {
    /// A Discord post target: a channel id OR a thread id (a Discord thread is a
    /// channel — same `POST /channels/{id}/messages` endpoint). The Patom-thread
    /// binding key.
    ContainerId, "discord_container_id"
}

discord_snowflake_newtype! {
    /// A Discord application (bot) snowflake. One app == one bot == one agent.
    ApplicationId, "discord_application_id"
}

discord_snowflake_newtype! {
    /// A Discord message snowflake. The live-event / backfill dedup key.
    DiscordMessageId, "discord_message_id"
}

discord_snowflake_newtype! {
    /// A Discord user snowflake — GLOBAL (not tenant-scoped). The stable identity
    /// key for the people directory.
    DiscordUserId, "discord_user_id"
}

// ─────────────────────────────────────────────────────────────────────────
// Secrets
// ─────────────────────────────────────────────────────────────────────────

/// A Discord bot token. `Debug`/`Display` redact; the bytes are only available
/// through [`BotToken::expose`].
///
/// No prefix is enforced: Discord bot tokens are opaque base64 with no stable,
/// documented leading marker (the historical `M…`/`mfa.…` shapes are not a
/// contract). We gate non-empty + length only, exactly like a secret.
#[derive(Clone)]
pub struct BotToken(SecretString);

impl BotToken {
    #[must_use]
    pub fn expose(&self) -> &str {
        self.0.expose()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl TryFrom<String> for BotToken {
    type Error = ParseError;

    fn try_from(raw: String) -> Result<Self, Self::Error> {
        let trimmed = raw.trim().to_owned();
        if trimmed.is_empty() {
            return Err(ParseError::Empty {
                field: "discord_bot_token",
            });
        }
        if trimmed.len() > DISCORD_TOKEN_MAX_LEN {
            return Err(ParseError::TooLong {
                field: "discord_bot_token",
                max: DISCORD_TOKEN_MAX_LEN,
                got: trimmed.len(),
            });
        }
        // A token with whitespace or control chars would corrupt the
        // `Authorization: Bot <token>` header — reject at the boundary.
        if trimmed
            .bytes()
            .any(|b| b.is_ascii_whitespace() || b.is_ascii_control())
        {
            return Err(ParseError::Malformed {
                field: "discord_bot_token",
                detail: "contains whitespace or control characters",
            });
        }
        Ok(Self(SecretString::try_from(trimmed).map_err(|_| {
            ParseError::Empty {
                field: "discord_bot_token",
            }
        })?))
    }
}

impl fmt::Debug for BotToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("BotToken(***)")
    }
}

// ─────────────────────────────────────────────────────────────────────────
// Gateway intents (bitfield — built only from named flags)
// ─────────────────────────────────────────────────────────────────────────

/// The Gateway intents bitfield (`IDENTIFY.intents`).
///
/// Deliberately **not** constructible from a raw `u64`: a hand-miscalculated
/// mask is closed by the Gateway with code 4013 (invalid intents), so the only
/// way to build an `Intents` is to OR named flags. [`Intents::DEFAULT`] is the
/// Patom set; widen via [`Intents::with`].
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Intents(u64);

impl Intents {
    /// `GUILDS` (1<<0) — channel/thread topology + thread events. Non-privileged.
    pub const GUILDS: Self = Self(1 << 0);
    /// `GUILD_MEMBERS` (1<<1) — **privileged**; roster of silent members + member deltas.
    pub const GUILD_MEMBERS: Self = Self(1 << 1);
    /// `GUILD_MESSAGES` (1<<9) — deliver `MESSAGE_CREATE` in guild channels. Non-privileged.
    pub const GUILD_MESSAGES: Self = Self(1 << 9);
    /// `DIRECT_MESSAGES` (1<<12) — deliver `MESSAGE_CREATE` in DMs. Non-privileged.
    pub const DIRECT_MESSAGES: Self = Self(1 << 12);
    /// `MESSAGE_CONTENT` (1<<15) — **privileged**; fills message text. The ambient-ingest gate.
    pub const MESSAGE_CONTENT: Self = Self(1 << 15);

    /// The default Patom intent set: topology + members + guild & DM messages +
    /// message content. Presence is deliberately omitted (an agent never needs
    /// online/offline status — minimizes privileged-data exposure).
    pub const DEFAULT: Self = Self(
        Self::GUILDS.0
            | Self::GUILD_MEMBERS.0
            | Self::GUILD_MESSAGES.0
            | Self::DIRECT_MESSAGES.0
            | Self::MESSAGE_CONTENT.0,
    );

    /// The raw bitfield for the `IDENTIFY` payload.
    #[must_use]
    pub const fn bits(self) -> u64 {
        self.0
    }

    /// Union with another flag/set.
    #[must_use]
    pub const fn with(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    /// Whether every bit of `other` is set.
    #[must_use]
    pub const fn has(self, other: Self) -> bool {
        (self.0 & other.0) == other.0
    }
}

impl fmt::Debug for Intents {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Intents(0b{:b})", self.0)
    }
}

// `Intents` serializes as the raw u64 the Gateway expects; there is no
// `Deserialize` (we never read an intents value back off the wire).
impl Serialize for Intents {
    fn serialize<S: Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
        ser.serialize_u64(self.0)
    }
}

// ─────────────────────────────────────────────────────────────────────────
// Gateway close-code classification (a typed enum, never a bare int)
// ─────────────────────────────────────────────────────────────────────────

/// What to do when the Gateway socket closes, derived from the close code.
///
/// The classification is **load-bearing**: a reconnect loop on a fatal code
/// (4013 invalid intents, 4014 disallowed intents) hammers the Gateway forever —
/// the fix is to correct the bitmask or enable the intent in the portal, not to
/// retry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloseAction {
    /// A clean close (1000/1001) — shut the connection task down, do not retry.
    Normal,
    /// Recoverable: back off and resume / re-identify.
    Reconnect,
    /// Unrecoverable config/auth error — surface a typed admin error, stop the loop.
    Fatal(FatalClose),
}

/// The specific fatal Gateway close codes (no automatic recovery).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FatalClose {
    /// 4004 — authentication failed (bad/blank bot token).
    AuthFailed,
    /// 4010 — an invalid shard was sent in IDENTIFY.
    InvalidShard,
    /// 4011 — the session would have handled too many guilds; sharding required.
    ShardingRequired,
    /// 4012 — invalid API version.
    InvalidApiVersion,
    /// 4013 — invalid intents (a miscalculated bitmask).
    InvalidIntents,
    /// 4014 — disallowed intents (a privileged intent not enabled/approved in the portal).
    DisallowedIntents,
}

impl FatalClose {
    /// The numeric close code.
    #[must_use]
    pub const fn code(self) -> u16 {
        match self {
            Self::AuthFailed => 4004,
            Self::InvalidShard => 4010,
            Self::ShardingRequired => 4011,
            Self::InvalidApiVersion => 4012,
            Self::InvalidIntents => 4013,
            Self::DisallowedIntents => 4014,
        }
    }
}

impl fmt::Display for FatalClose {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let reason = match self {
            Self::AuthFailed => "authentication failed (bad bot token)",
            Self::InvalidShard => "invalid shard",
            Self::ShardingRequired => "sharding required",
            Self::InvalidApiVersion => "invalid API version",
            Self::InvalidIntents => "invalid intents (miscalculated bitmask)",
            Self::DisallowedIntents => {
                "disallowed intents (enable the privileged intent in the Developer Portal)"
            }
        };
        write!(f, "{} (close {})", reason, self.code())
    }
}

/// Classify a WebSocket close code into the recovery action.
///
/// A `None` code (an abnormal close with no code — TCP reset, zombie detect)
/// is treated as reconnectable: the socket dropped, not the credentials.
#[must_use]
pub fn classify_close(code: Option<u16>) -> CloseAction {
    match code {
        // Clean closes.
        Some(1000 | 1001) => CloseAction::Normal,
        // Fatal config/auth — do NOT reconnect.
        Some(4004) => CloseAction::Fatal(FatalClose::AuthFailed),
        Some(4010) => CloseAction::Fatal(FatalClose::InvalidShard),
        Some(4011) => CloseAction::Fatal(FatalClose::ShardingRequired),
        Some(4012) => CloseAction::Fatal(FatalClose::InvalidApiVersion),
        Some(4013) => CloseAction::Fatal(FatalClose::InvalidIntents),
        Some(4014) => CloseAction::Fatal(FatalClose::DisallowedIntents),
        // No code (zombie / transport drop) or any reconnectable code
        // (4000/4001/4002/4003/4005/4007/4008/4009) → back off and reconnect.
        _ => CloseAction::Reconnect,
    }
}

// ─────────────────────────────────────────────────────────────────────────
// Channel types
// ─────────────────────────────────────────────────────────────────────────

/// Discord channel types (the `type` field on a channel object).
///
/// Only the values Patom reasons about are named; any future type round-trips
/// through [`ChannelType::Unknown`] (a new Discord type is an operating
/// condition, not a programmer error — §6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChannelType {
    GuildText,
    Dm,
    GuildVoice,
    GroupDm,
    GuildCategory,
    GuildAnnouncement,
    AnnouncementThread,
    PublicThread,
    PrivateThread,
    GuildStageVoice,
    GuildDirectory,
    GuildForum,
    GuildMedia,
    Unknown(u8),
}

impl ChannelType {
    #[must_use]
    pub const fn from_code(code: u8) -> Self {
        match code {
            0 => Self::GuildText,
            1 => Self::Dm,
            2 => Self::GuildVoice,
            3 => Self::GroupDm,
            4 => Self::GuildCategory,
            5 => Self::GuildAnnouncement,
            10 => Self::AnnouncementThread,
            11 => Self::PublicThread,
            12 => Self::PrivateThread,
            13 => Self::GuildStageVoice,
            14 => Self::GuildDirectory,
            15 => Self::GuildForum,
            16 => Self::GuildMedia,
            other => Self::Unknown(other),
        }
    }

    #[must_use]
    pub const fn code(self) -> u8 {
        match self {
            Self::GuildText => 0,
            Self::Dm => 1,
            Self::GuildVoice => 2,
            Self::GroupDm => 3,
            Self::GuildCategory => 4,
            Self::GuildAnnouncement => 5,
            Self::AnnouncementThread => 10,
            Self::PublicThread => 11,
            Self::PrivateThread => 12,
            Self::GuildStageVoice => 13,
            Self::GuildDirectory => 14,
            Self::GuildForum => 15,
            Self::GuildMedia => 16,
            Self::Unknown(other) => other,
        }
    }

    /// Whether this is one of the thread channel types (announcement / public /
    /// private). A thread binds to its own container id; its parent is the
    /// grouping channel.
    #[must_use]
    pub const fn is_thread(self) -> bool {
        matches!(
            self,
            Self::AnnouncementThread | Self::PublicThread | Self::PrivateThread
        )
    }

    /// Whether this is a direct-message channel (1:1 or group). DMs trigger on
    /// every message and deliver content without the `MESSAGE_CONTENT` intent.
    #[must_use]
    pub const fn is_dm(self) -> bool {
        matches!(self, Self::Dm | Self::GroupDm)
    }
}

impl Serialize for ChannelType {
    fn serialize<S: Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
        ser.serialize_u8(self.code())
    }
}

impl<'de> Deserialize<'de> for ChannelType {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        let raw = u8::deserialize(de)?;
        Ok(Self::from_code(raw))
    }
}

// ─────────────────────────────────────────────────────────────────────────
// Shared parsing helper
// ─────────────────────────────────────────────────────────────────────────

/// Digits-only gate for a Discord snowflake. Snowflakes are decimal `u64`
/// strings; reject anything else so a hostile frame cannot smuggle markup, a
/// SQL fragment, or an oversized blob past the boundary.
fn parse_snowflake<'a>(raw: &'a str, field: &'static str) -> Result<&'a str, ParseError> {
    if raw.is_empty() {
        return Err(ParseError::Empty { field });
    }
    if raw.len() > DISCORD_SNOWFLAKE_MAX_LEN {
        return Err(ParseError::TooLong {
            field,
            max: DISCORD_SNOWFLAKE_MAX_LEN,
            got: raw.len(),
        });
    }
    if !raw.bytes().all(|b| b.is_ascii_digit()) {
        return Err(ParseError::Malformed {
            field,
            detail: "snowflake must be ASCII digits",
        });
    }
    Ok(raw)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snowflake_accepts_typical() {
        let g = GuildId::try_from("974519864045756446").expect("valid guild id");
        assert_eq!(g.as_str(), "974519864045756446");
    }

    #[test]
    fn snowflake_rejects_empty_too_long_and_non_digit() {
        assert!(DiscordUserId::try_from("").is_err());
        let big = "1".repeat(DISCORD_SNOWFLAKE_MAX_LEN + 1);
        assert!(DiscordUserId::try_from(big.as_str()).is_err());
        assert!(ContainerId::try_from("oc_abc").is_err());
        assert!(GuildId::try_from("123;DROP").is_err());
        assert!(GuildId::try_from("12 34").is_err());
    }

    #[test]
    fn snowflake_serde_reads_json_string_not_number() {
        let u = DiscordUserId::try_from("80351110224678912").expect("valid");
        let json = serde_json::to_string(&u).expect("ser");
        assert_eq!(json, "\"80351110224678912\"");
        let back: DiscordUserId = serde_json::from_str(&json).expect("de");
        assert_eq!(back, u);
        // A bare JSON number must NOT deserialize — that is the f64-corruption path.
        let as_number: Result<DiscordUserId, _> = serde_json::from_str("80351110224678912");
        assert!(as_number.is_err());
    }

    #[test]
    fn bot_token_redacts_debug_and_trims() {
        let t = BotToken::try_from("  MTk4N.example.token  ".to_owned()).expect("valid");
        let dbg = format!("{t:?}");
        assert_eq!(dbg, "BotToken(***)");
        assert!(!dbg.contains("example"));
        assert_eq!(t.expose(), "MTk4N.example.token");
        assert!(!t.is_empty());
    }

    #[test]
    fn bot_token_rejects_empty_whitespace_and_too_long() {
        assert!(BotToken::try_from(String::new()).is_err());
        assert!(BotToken::try_from("   ".to_owned()).is_err());
        assert!(BotToken::try_from("has space".to_owned()).is_err());
        let big = "a".repeat(DISCORD_TOKEN_MAX_LEN + 1);
        assert!(BotToken::try_from(big).is_err());
    }

    #[test]
    fn intents_default_has_all_five_and_serializes_to_u64() {
        let d = Intents::DEFAULT;
        assert!(d.has(Intents::GUILDS));
        assert!(d.has(Intents::GUILD_MEMBERS));
        assert!(d.has(Intents::GUILD_MESSAGES));
        assert!(d.has(Intents::DIRECT_MESSAGES));
        assert!(d.has(Intents::MESSAGE_CONTENT));
        // 1 + 2 + 512 + 4096 + 32768 = 37379.
        assert_eq!(d.bits(), 37379);
        assert_eq!(serde_json::to_string(&d).expect("ser"), "37379");
    }

    #[test]
    fn intents_with_is_a_union() {
        let only_guilds = Intents::GUILDS;
        assert!(!only_guilds.has(Intents::MESSAGE_CONTENT));
        let widened = only_guilds.with(Intents::MESSAGE_CONTENT);
        assert!(widened.has(Intents::GUILDS));
        assert!(widened.has(Intents::MESSAGE_CONTENT));
    }

    #[test]
    fn close_codes_classify_fatal_vs_reconnect_vs_normal() {
        assert_eq!(classify_close(Some(1000)), CloseAction::Normal);
        assert_eq!(classify_close(Some(1001)), CloseAction::Normal);
        assert_eq!(
            classify_close(Some(4014)),
            CloseAction::Fatal(FatalClose::DisallowedIntents)
        );
        assert_eq!(
            classify_close(Some(4013)),
            CloseAction::Fatal(FatalClose::InvalidIntents)
        );
        assert_eq!(
            classify_close(Some(4004)),
            CloseAction::Fatal(FatalClose::AuthFailed)
        );
        // The documented reconnectable set.
        for code in [4000u16, 4001, 4002, 4003, 4005, 4007, 4008, 4009] {
            assert_eq!(classify_close(Some(code)), CloseAction::Reconnect);
        }
        // No code (zombie / transport drop) → reconnect.
        assert_eq!(classify_close(None), CloseAction::Reconnect);
    }

    #[test]
    fn fatal_close_code_roundtrip() {
        assert_eq!(FatalClose::DisallowedIntents.code(), 4014);
        assert_eq!(FatalClose::AuthFailed.code(), 4004);
        assert!(format!("{}", FatalClose::DisallowedIntents).contains("4014"));
    }

    #[test]
    fn channel_type_known_and_unknown_roundtrip() {
        assert_eq!(ChannelType::from_code(0), ChannelType::GuildText);
        assert_eq!(ChannelType::from_code(11), ChannelType::PublicThread);
        assert!(ChannelType::from_code(11).is_thread());
        assert!(ChannelType::from_code(1).is_dm());
        assert!(!ChannelType::from_code(0).is_thread());
        assert_eq!(ChannelType::from_code(99), ChannelType::Unknown(99));
        assert_eq!(ChannelType::from_code(99).code(), 99);
        // serde round-trip through the numeric wire form.
        let json = serde_json::to_string(&ChannelType::PublicThread).expect("ser");
        assert_eq!(json, "11");
        let back: ChannelType = serde_json::from_str("11").expect("de");
        assert_eq!(back, ChannelType::PublicThread);
    }
}
