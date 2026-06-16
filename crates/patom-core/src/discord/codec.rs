//! Gateway JSON codec — the `{op, d, s, t}` envelope (CLAUDE.md §1: serde only
//! at the boundary).
//!
//! Discord's Gateway speaks JSON text frames (`?v=10&encoding=json`), so the
//! codec is plain `serde_json` — no protobuf, no fragment reassembly (the two
//! simplifications over Lark's `pbbp2`). Inbound frames decode to [`GatewayRecv`]
//! (opcode + optional seq/event-name/data); outbound control frames encode from
//! a typed payload via [`encode_command`].

use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::types::ParseError;

use super::error::DiscordError;
use super::limits::DISCORD_CONTROL_PAYLOAD_MAX_BYTES;
use super::types::DiscordUserId;

/// A Gateway opcode (the `op` field). Verified against
/// `discord.com/developers/docs/events/gateway`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Opcode {
    /// 0 — an event was dispatched (server→client). Carries `s` + `t` + `d`.
    Dispatch,
    /// 1 — keep-alive (both directions).
    Heartbeat,
    /// 2 — start a new session (client→server).
    Identify,
    /// 6 — resume a dropped session (client→server).
    Resume,
    /// 7 — the server asks us to reconnect (server→client).
    Reconnect,
    /// 8 — request guild members (client→server).
    RequestGuildMembers,
    /// 9 — the session is invalid; `d` is a bool (resumable?) (server→client).
    InvalidSession,
    /// 10 — the first frame after connect; `d.heartbeat_interval` (server→client).
    Hello,
    /// 11 — acknowledges our heartbeat (server→client).
    HeartbeatAck,
    /// Any opcode we do not act on (3 presence, 4 voice, …). Ignored on receive.
    Other(u8),
}

impl Opcode {
    #[must_use]
    pub const fn from_u8(raw: u8) -> Self {
        match raw {
            0 => Self::Dispatch,
            1 => Self::Heartbeat,
            2 => Self::Identify,
            6 => Self::Resume,
            7 => Self::Reconnect,
            8 => Self::RequestGuildMembers,
            9 => Self::InvalidSession,
            10 => Self::Hello,
            11 => Self::HeartbeatAck,
            other => Self::Other(other),
        }
    }

    #[must_use]
    pub const fn as_u8(self) -> u8 {
        match self {
            Self::Dispatch => 0,
            Self::Heartbeat => 1,
            Self::Identify => 2,
            Self::Resume => 6,
            Self::Reconnect => 7,
            Self::RequestGuildMembers => 8,
            Self::InvalidSession => 9,
            Self::Hello => 10,
            Self::HeartbeatAck => 11,
            Self::Other(other) => other,
        }
    }
}

/// A decoded inbound Gateway frame.
///
/// `data` is held as a raw [`serde_json::Value`] and re-parsed per opcode/event
/// (`parse_hello`, `parse_ready`, or `event::parse_event` for a dispatch) — the
/// codec stays agnostic to the event zoo.
#[derive(Debug, Clone)]
pub struct GatewayRecv {
    pub op: Opcode,
    /// Sequence number — present only on a `Dispatch` (the heartbeat `d`).
    pub seq: Option<u64>,
    /// Event name (`MESSAGE_CREATE`, `READY`, …) — present only on a `Dispatch`.
    pub event_type: Option<String>,
    /// The opcode-specific payload.
    pub data: Option<serde_json::Value>,
}

impl GatewayRecv {
    /// Re-parse `data` into a typed payload (e.g. a dispatch event struct).
    /// `None` data is a decode error — the caller asked for a payload that the
    /// frame did not carry.
    pub fn parse_data<T: DeserializeOwned>(&self) -> Result<T, DiscordError> {
        let value = self.data.as_ref().ok_or_else(|| {
            DiscordError::Gateway(format!("op {} carried no data", self.op.as_u8()))
        })?;
        serde_json::from_value(value.clone()).map_err(DiscordError::from)
    }
}

/// The raw envelope shape, decoded before the opcode is interpreted.
#[derive(serde::Deserialize)]
struct RawEnvelope {
    op: u8,
    #[serde(default)]
    d: Option<serde_json::Value>,
    #[serde(default)]
    s: Option<u64>,
    #[serde(default)]
    t: Option<String>,
}

/// Decode an inbound Gateway text frame.
pub fn decode(bytes: &[u8]) -> Result<GatewayRecv, DiscordError> {
    let env: RawEnvelope = serde_json::from_slice(bytes)?;
    Ok(GatewayRecv {
        op: Opcode::from_u8(env.op),
        seq: env.s,
        event_type: env.t,
        data: env.d,
    })
}

/// Encode an outbound control command (`{op, d}`) from a typed payload.
///
/// Asserts the serialized control payload stays under Discord's 4096-byte cap
/// (§5/§6): exceeding it is a payload-construction bug, not an operating error.
pub fn encode_command<T: Serialize>(op: Opcode, data: &T) -> Result<String, DiscordError> {
    #[derive(Serialize)]
    struct Out<'a, T> {
        op: u8,
        d: &'a T,
    }
    let text = serde_json::to_string(&Out {
        op: op.as_u8(),
        d: data,
    })?;
    assert!(
        text.len() <= DISCORD_CONTROL_PAYLOAD_MAX_BYTES,
        "control payload {} bytes exceeds the {DISCORD_CONTROL_PAYLOAD_MAX_BYTES}-byte Gateway cap",
        text.len(),
    );
    Ok(text)
}

// ─────────────────────────────────────────────────────────────────────────
// Protocol payloads parsed by the connection loop
// ─────────────────────────────────────────────────────────────────────────

/// `HELLO` (op 10) — the heartbeat cadence the server dictates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Hello {
    pub heartbeat_interval_ms: u64,
}

/// Parse the `HELLO` payload.
pub fn parse_hello(recv: &GatewayRecv) -> Result<Hello, DiscordError> {
    #[derive(serde::Deserialize)]
    struct Raw {
        heartbeat_interval: u64,
    }
    let raw: Raw = recv.parse_data()?;
    if raw.heartbeat_interval == 0 {
        return Err(DiscordError::Gateway(
            "hello heartbeat_interval was 0".to_owned(),
        ));
    }
    Ok(Hello {
        heartbeat_interval_ms: raw.heartbeat_interval,
    })
}

/// Maximum length of a Gateway `session_id` / `resume_gateway_url`. Opaque
/// server-issued strings; cap them so a hostile frame cannot smuggle a blob.
pub const DISCORD_SESSION_FIELD_MAX_LEN: usize = 512;

/// `READY` (op 0, `t = READY`) — the session identity needed to RESUME, plus the
/// bot's own user snowflake (so the bridge can drop the bot's own re-delivered
/// messages).
#[derive(Debug, Clone)]
pub struct Ready {
    pub session_id: String,
    pub resume_gateway_url: String,
    pub bot_user_id: DiscordUserId,
}

/// Parse the `READY` dispatch payload.
pub fn parse_ready(recv: &GatewayRecv) -> Result<Ready, DiscordError> {
    #[derive(serde::Deserialize)]
    struct RawUser {
        id: DiscordUserId,
    }
    #[derive(serde::Deserialize)]
    struct Raw {
        session_id: String,
        resume_gateway_url: String,
        user: RawUser,
    }
    let raw: Raw = recv.parse_data()?;
    let session_id = bounded_session_field(raw.session_id, "discord_session_id")?;
    let resume_gateway_url =
        bounded_session_field(raw.resume_gateway_url, "discord_resume_gateway_url")?;
    Ok(Ready {
        session_id,
        resume_gateway_url,
        bot_user_id: raw.user.id,
    })
}

fn bounded_session_field(raw: String, field: &'static str) -> Result<String, DiscordError> {
    if raw.is_empty() {
        return Err(DiscordError::Parse(ParseError::Empty { field }));
    }
    if raw.len() > DISCORD_SESSION_FIELD_MAX_LEN {
        return Err(DiscordError::Parse(ParseError::TooLong {
            field,
            max: DISCORD_SESSION_FIELD_MAX_LEN,
            got: raw.len(),
        }));
    }
    Ok(raw)
}

/// `INVALID_SESSION` (op 9) — `d` is a bool: whether the session may be resumed.
/// A non-bool / absent `d` is conservatively treated as **not** resumable.
#[must_use]
pub fn parse_invalid_session_resumable(recv: &GatewayRecv) -> bool {
    recv.data
        .as_ref()
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opcode_roundtrips_known_and_unknown() {
        for raw in [0u8, 1, 2, 6, 7, 8, 9, 10, 11] {
            assert_eq!(Opcode::from_u8(raw).as_u8(), raw);
        }
        assert_eq!(Opcode::from_u8(3), Opcode::Other(3));
        assert_eq!(Opcode::from_u8(99).as_u8(), 99);
    }

    #[test]
    fn decode_dispatch_frame_carries_seq_and_type() {
        let frame = br#"{"op":0,"s":42,"t":"MESSAGE_CREATE","d":{"content":"hi"}}"#;
        let recv = decode(frame).expect("decode");
        assert_eq!(recv.op, Opcode::Dispatch);
        assert_eq!(recv.seq, Some(42));
        assert_eq!(recv.event_type.as_deref(), Some("MESSAGE_CREATE"));
        assert!(recv.data.is_some());
    }

    #[test]
    fn decode_hello_has_no_seq_or_type() {
        let recv = decode(br#"{"op":10,"d":{"heartbeat_interval":41250}}"#).expect("decode");
        assert_eq!(recv.op, Opcode::Hello);
        assert_eq!(recv.seq, None);
        assert_eq!(recv.event_type, None);
        let hello = parse_hello(&recv).expect("hello");
        assert_eq!(hello.heartbeat_interval_ms, 41250);
    }

    #[test]
    fn hello_rejects_zero_interval() {
        let recv = decode(br#"{"op":10,"d":{"heartbeat_interval":0}}"#).expect("decode");
        assert!(parse_hello(&recv).is_err());
    }

    #[test]
    fn decode_reconnect_and_heartbeat_ack_have_null_data() {
        let recon = decode(br#"{"op":7,"d":null}"#).expect("decode");
        assert_eq!(recon.op, Opcode::Reconnect);
        let ack = decode(br#"{"op":11,"d":null}"#).expect("decode");
        assert_eq!(ack.op, Opcode::HeartbeatAck);
    }

    #[test]
    fn parse_ready_extracts_session_and_bot_user() {
        let frame = br#"{"op":0,"t":"READY","s":1,"d":{
            "session_id":"abc123session",
            "resume_gateway_url":"wss://gateway-us-east1-b.discord.gg",
            "user":{"id":"80351110224678912","username":"patombot"}
        }}"#;
        let recv = decode(frame).expect("decode");
        let ready = parse_ready(&recv).expect("ready");
        assert_eq!(ready.session_id, "abc123session");
        assert_eq!(
            ready.resume_gateway_url,
            "wss://gateway-us-east1-b.discord.gg"
        );
        assert_eq!(ready.bot_user_id.as_str(), "80351110224678912");
    }

    #[test]
    fn parse_ready_rejects_a_non_snowflake_bot_id() {
        // A non-digit user id (the f64-corruption guard) is rejected at the boundary.
        let frame = br#"{"op":0,"t":"READY","d":{
            "session_id":"s","resume_gateway_url":"wss://x","user":{"id":"not-a-snowflake"}
        }}"#;
        let recv = decode(frame).expect("decode");
        assert!(parse_ready(&recv).is_err());
    }

    #[test]
    fn invalid_session_resumable_flag() {
        let yes = decode(br#"{"op":9,"d":true}"#).expect("decode");
        assert!(parse_invalid_session_resumable(&yes));
        let no = decode(br#"{"op":9,"d":false}"#).expect("decode");
        assert!(!parse_invalid_session_resumable(&no));
        // Absent / non-bool d → conservatively NOT resumable.
        let weird = decode(br#"{"op":9,"d":null}"#).expect("decode");
        assert!(!parse_invalid_session_resumable(&weird));
    }

    #[test]
    fn encode_command_wraps_op_and_data() {
        #[derive(Serialize)]
        struct Beat(Option<u64>);
        let text = encode_command(Opcode::Heartbeat, &Beat(Some(251))).expect("encode");
        // `{"op":1,"d":251}`
        let v: serde_json::Value = serde_json::from_str(&text).expect("json");
        assert_eq!(v["op"], 1);
        assert_eq!(v["d"], 251);
    }

    #[test]
    fn decode_rejects_malformed_json() {
        assert!(decode(b"not json").is_err());
        assert!(decode(br#"{"no_op_field":1}"#).is_err());
    }
}
