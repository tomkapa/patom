//! Gateway bootstrap: `GET /gateway/bot`, the connect-URL builder, and the
//! outbound control-command builders (IDENTIFY / RESUME / HEARTBEAT /
//! REQUEST_GUILD_MEMBERS).
//!
//! `GET /gateway/bot` returns the WSS URL plus the session-start limits; the
//! caller dials `{url}/?v=10&encoding=json` (via [`connect_url`]). The command
//! builders serialize through `codec::encode_command` so the `{op, d}` envelope
//! and the 4096-byte cap are enforced in one place.

use serde::Deserialize;

use super::codec::{self, Opcode};
use super::error::DiscordError;
use super::limits::{DISCORD_GATEWAY_BOT_TIMEOUT, DISCORD_GATEWAY_QUERY};
use super::types::{BotToken, GuildId, Intents};

/// The `GET /gateway/bot` response: where to connect + how many sessions remain.
#[derive(Debug, Clone, Deserialize)]
pub struct GatewayInfo {
    /// The base WSS URL, e.g. `wss://gateway.discord.gg`.
    pub url: String,
    /// Recommended shard count (1 at the experiment's scale).
    #[serde(default)]
    pub shards: u32,
    pub session_start_limit: SessionStartLimit,
}

/// The remaining IDENTIFY budget (1000/24h) + the concurrency bucket size.
#[derive(Debug, Clone, Copy, Deserialize)]
pub struct SessionStartLimit {
    pub total: u32,
    pub remaining: u32,
    /// Milliseconds until `remaining` resets to `total`.
    pub reset_after: u64,
    /// How many shards may IDENTIFY concurrently (bucketed by `shard_id % this`).
    pub max_concurrency: u32,
}

/// Fetch `GET /gateway/bot` with the bot token.
pub async fn get_gateway_bot(
    http: &reqwest::Client,
    api_base: &str,
    token: &BotToken,
) -> Result<GatewayInfo, DiscordError> {
    let url = format!("{api_base}/gateway/bot");
    let send = http
        .get(&url)
        .header("Authorization", format!("Bot {}", token.expose()))
        .send();
    let resp = tokio::time::timeout(DISCORD_GATEWAY_BOT_TIMEOUT, send)
        .await
        .map_err(|_| DiscordError::Gateway("GET /gateway/bot timed out".to_owned()))??;
    let status = resp.status();
    let bytes = resp.bytes().await?;
    if !status.is_success() {
        // 401 here == a bad/blank bot token (the static-token re-credential path).
        return Err(DiscordError::PostFailed {
            status: status.as_u16(),
            body: String::from_utf8_lossy(&bytes).chars().take(512).collect(),
        });
    }
    serde_json::from_slice(&bytes).map_err(DiscordError::from)
}

/// Build the WSS connect URL from a base (the `GET /gateway/bot` `url` or a
/// READY `resume_gateway_url`): `{base}/?v=10&encoding=json`.
#[must_use]
pub fn connect_url(base: &str) -> String {
    format!("{}/?{}", base.trim_end_matches('/'), DISCORD_GATEWAY_QUERY)
}

// ─────────────────────────────────────────────────────────────────────────
// Outbound control commands
// ─────────────────────────────────────────────────────────────────────────

#[derive(serde::Serialize)]
struct IdentifyProperties {
    os: &'static str,
    browser: &'static str,
    device: &'static str,
}

#[derive(serde::Serialize)]
struct Identify<'a> {
    token: &'a str,
    intents: Intents,
    properties: IdentifyProperties,
    #[serde(skip_serializing_if = "Option::is_none")]
    shard: Option<[u32; 2]>,
}

/// Build an `IDENTIFY` (op 2) command. `shard` is pre-designed for multi-shard
/// (a config change, not a rewrite) but is `None` at the experiment's scale.
pub fn identify(
    token: &BotToken,
    intents: Intents,
    shard: Option<[u32; 2]>,
) -> Result<String, DiscordError> {
    codec::encode_command(
        Opcode::Identify,
        &Identify {
            token: token.expose(),
            intents,
            properties: IdentifyProperties {
                os: std::env::consts::OS,
                browser: "patom",
                device: "patom",
            },
            shard,
        },
    )
}

#[derive(serde::Serialize)]
struct Resume<'a> {
    token: &'a str,
    session_id: &'a str,
    seq: u64,
}

/// Build a `RESUME` (op 6) command to replay missed events on a dropped session.
pub fn resume(token: &BotToken, session_id: &str, seq: u64) -> Result<String, DiscordError> {
    codec::encode_command(
        Opcode::Resume,
        &Resume {
            token: token.expose(),
            session_id,
            seq,
        },
    )
}

/// Build a `HEARTBEAT` (op 1) command. `d` is the last sequence we received
/// (`null` before the first dispatch).
pub fn heartbeat(last_seq: Option<u64>) -> Result<String, DiscordError> {
    codec::encode_command(Opcode::Heartbeat, &last_seq)
}

#[derive(serde::Serialize)]
struct RequestGuildMembers<'a> {
    guild_id: &'a str,
    query: &'static str,
    limit: u32,
}

/// Build a `REQUEST_GUILD_MEMBERS` (op 8) command for the full roster.
///
/// `query=""`, `limit=0` asks for everyone. Used once for the initial sync;
/// thereafter the roster is kept warm by member-delta events (a Oct-2025 rate
/// limit caps this op).
pub fn request_guild_members(guild_id: &GuildId) -> Result<String, DiscordError> {
    codec::encode_command(
        Opcode::RequestGuildMembers,
        &RequestGuildMembers {
            guild_id: guild_id.as_str(),
            query: "",
            limit: 0,
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn token() -> BotToken {
        BotToken::try_from("MTk4N.example.token".to_owned()).expect("valid token")
    }

    #[test]
    fn connect_url_appends_version_and_encoding() {
        assert_eq!(
            connect_url("wss://gateway.discord.gg"),
            "wss://gateway.discord.gg/?v=10&encoding=json"
        );
        // A trailing slash does not double up.
        assert_eq!(
            connect_url("wss://gateway-us-east1-b.discord.gg/"),
            "wss://gateway-us-east1-b.discord.gg/?v=10&encoding=json"
        );
    }

    #[test]
    fn identify_carries_token_intents_and_properties() {
        let text = identify(&token(), Intents::DEFAULT, None).expect("identify");
        let v: serde_json::Value = serde_json::from_str(&text).expect("json");
        assert_eq!(v["op"], 2);
        assert_eq!(v["d"]["token"], "MTk4N.example.token");
        assert_eq!(v["d"]["intents"], 37379);
        assert!(v["d"]["properties"]["browser"] == "patom");
        // shard omitted at single-shard scale.
        assert!(v["d"].get("shard").is_none());
    }

    #[test]
    fn identify_includes_shard_when_set() {
        let text = identify(&token(), Intents::DEFAULT, Some([0, 1])).expect("identify");
        let v: serde_json::Value = serde_json::from_str(&text).expect("json");
        assert_eq!(v["d"]["shard"][0], 0);
        assert_eq!(v["d"]["shard"][1], 1);
    }

    #[test]
    fn resume_carries_session_and_seq() {
        let text = resume(&token(), "sess-abc", 99).expect("resume");
        let v: serde_json::Value = serde_json::from_str(&text).expect("json");
        assert_eq!(v["op"], 6);
        assert_eq!(v["d"]["session_id"], "sess-abc");
        assert_eq!(v["d"]["seq"], 99);
    }

    #[test]
    fn heartbeat_null_and_seq_forms() {
        let none = heartbeat(None).expect("hb");
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&none).expect("json")["d"],
            serde_json::Value::Null
        );
        let some = heartbeat(Some(7)).expect("hb");
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&some).expect("json")["d"],
            7
        );
    }

    #[test]
    fn request_guild_members_asks_for_everyone() {
        let g = GuildId::try_from("974519864045756446").expect("guild");
        let text = request_guild_members(&g).expect("rgm");
        let v: serde_json::Value = serde_json::from_str(&text).expect("json");
        assert_eq!(v["op"], 8);
        assert_eq!(v["d"]["guild_id"], "974519864045756446");
        assert_eq!(v["d"]["query"], "");
        assert_eq!(v["d"]["limit"], 0);
    }
}
