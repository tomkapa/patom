//! pbbp2 — the protobuf frame of Lark's long-connection (WebSocket) transport.
//!
//! Hand-written `prost` messages (no `prost-build`/`protoc`; see
//! `proto/pbbp2.proto` for the reference wire format) plus the protocol
//! constants, transcribed from `larksuite/oapi-sdk-go` (`ws/`, branch
//! `v3_main`). The reassembler that stitches fragmented frames lives in
//! [`super::codec`].
//!
//! Two distinct code spaces (a real footgun — they are NOT the same):
//! - the **handshake** `EndpointResp.code`: `0` == OK (see [`ENDPOINT_OK`]);
//! - the **data-frame ACK** `Response.code`: `200` == OK (an HTTP status; see
//!   [`ACK_OK`]). The SDK builds the ACK with `NewResponseByCode(http.StatusOK)`.

use prost::Message as _;
use thiserror::Error;

// ── Frame.method ────────────────────────────────────────────────────────────
/// Control frame (ping / pong).
pub const METHOD_CONTROL: i32 = 0;
/// Data frame (event / card delivery + the upstream ACK).
pub const METHOD_DATA: i32 = 1;

// ── Header keys ─────────────────────────────────────────────────────────────
pub const HEADER_TYPE: &str = "type";
pub const HEADER_MESSAGE_ID: &str = "message_id";
pub const HEADER_SUM: &str = "sum";
pub const HEADER_SEQ: &str = "seq";
pub const HEADER_TRACE_ID: &str = "trace_id";
pub const HEADER_BIZ_RT: &str = "biz_rt";

// ── `type` header values ────────────────────────────────────────────────────
pub const TYPE_EVENT: &str = "event";
pub const TYPE_CARD: &str = "card";
pub const TYPE_PING: &str = "ping";
pub const TYPE_PONG: &str = "pong";

// ── Handshake `EndpointResp.code` values ────────────────────────────────────
pub const ENDPOINT_OK: i32 = 0;
pub const ENDPOINT_SYSTEM_BUSY: i32 = 1;
pub const ENDPOINT_FORBIDDEN: i32 = 403;
pub const ENDPOINT_AUTH_FAILED: i32 = 514;
pub const ENDPOINT_EXCEED_CONN_LIMIT: i32 = 1_000_040_350;
pub const ENDPOINT_INTERNAL_ERROR: i32 = 1_000_040_343;

// ── Data-frame ACK `Response.code` values (HTTP statuses) ───────────────────
pub const ACK_OK: i32 = 200;
pub const ACK_INTERNAL_ERROR: i32 = 500;

#[derive(Debug, Error)]
pub enum Pbbp2Error {
    #[error("decode: {0}")]
    Decode(#[from] prost::DecodeError),
    #[error("reassembly buffer overflow (cap {cap})")]
    ReassemblyOverflow { cap: usize },
    #[error("fragment header invalid: sum={sum} seq={seq}")]
    BadFragment { sum: i64, seq: i64 },
    #[error("missing required header: {0}")]
    MissingHeader(&'static str),
}

#[derive(Clone, PartialEq, Eq, ::prost::Message)]
pub struct Header {
    #[prost(string, tag = "1")]
    pub key: String,
    #[prost(string, tag = "2")]
    pub value: String,
}

impl Header {
    #[must_use]
    pub fn new(key: &str, value: impl Into<String>) -> Self {
        Self {
            key: key.to_owned(),
            value: value.into(),
        }
    }
}

#[derive(Clone, PartialEq, Eq, ::prost::Message)]
pub struct Frame {
    #[prost(uint64, tag = "1")]
    pub seq_id: u64,
    #[prost(uint64, tag = "2")]
    pub log_id: u64,
    #[prost(int32, tag = "3")]
    pub service: i32,
    #[prost(int32, tag = "4")]
    pub method: i32,
    #[prost(message, repeated, tag = "5")]
    pub headers: Vec<Header>,
    #[prost(string, tag = "6")]
    pub payload_encoding: String,
    #[prost(string, tag = "7")]
    pub payload_type: String,
    #[prost(bytes = "vec", tag = "8")]
    pub payload: Vec<u8>,
    #[prost(string, tag = "9")]
    pub log_id_new: String,
}

impl Frame {
    /// Decode a binary WS message into a frame.
    pub fn decode_bytes(buf: &[u8]) -> Result<Self, Pbbp2Error> {
        Self::decode(buf).map_err(Pbbp2Error::from)
    }

    /// Encode the frame to the bytes written on the WS binary channel.
    #[must_use]
    pub fn encode_to_bytes(&self) -> Vec<u8> {
        self.encode_to_vec()
    }

    /// First header value for `key`, if present.
    #[must_use]
    pub fn header(&self, key: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|h| h.key == key)
            .map(|h| h.value.as_str())
    }

    /// Integer header value for `key` (base-10), if present and parseable.
    #[must_use]
    pub fn header_int(&self, key: &str) -> Option<i64> {
        self.header(key).and_then(|v| v.parse::<i64>().ok())
    }

    /// The `type` header (`event` / `card` / `ping` / `pong`).
    #[must_use]
    pub fn msg_type(&self) -> Option<&str> {
        self.header(HEADER_TYPE)
    }

    /// The `message_id` header (the fragment-reassembly + ingest key).
    #[must_use]
    pub fn message_id(&self) -> Option<&str> {
        self.header(HEADER_MESSAGE_ID)
    }

    /// A client→server control ping (`method=Control`, `type=ping`), tagged with
    /// the connection's `service_id`.
    #[must_use]
    pub fn ping(service: i32) -> Self {
        Self {
            method: METHOD_CONTROL,
            service,
            headers: vec![Header::new(HEADER_TYPE, TYPE_PING)],
            ..Self::default()
        }
    }

    /// Turn a received data frame into its upstream ACK: echo the frame (method,
    /// service, headers) with a `biz_rt` header appended and the payload
    /// replaced by `{"code":<code>}` (the SDK's `Response`). `code` is an HTTP
    /// status — [`ACK_OK`] (200) on success.
    #[must_use]
    pub fn into_ack(mut self, code: i32, biz_rt_ms: i64) -> Self {
        self.headers
            .push(Header::new(HEADER_BIZ_RT, biz_rt_ms.to_string()));
        // The Response struct is `{"code":int,"headers":..,"data":..}`; both
        // optional fields are omitted on a bare ACK, so a hand-built literal is
        // exact and cannot fail to serialize.
        self.payload = format!("{{\"code\":{code}}}").into_bytes();
        self.payload_encoding.clear();
        self.payload_type.clear();
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_round_trips_through_prost() {
        let f = Frame {
            seq_id: 7,
            log_id: 9,
            service: 3,
            method: METHOD_DATA,
            headers: vec![
                Header::new(HEADER_TYPE, TYPE_EVENT),
                Header::new(HEADER_MESSAGE_ID, "om_123"),
            ],
            payload: b"hello".to_vec(),
            ..Frame::default()
        };
        let bytes = f.encode_to_bytes();
        let back = Frame::decode_bytes(&bytes).expect("decode");
        assert_eq!(back, f);
    }

    #[test]
    fn header_accessors_read_typed_values() {
        let f = Frame {
            headers: vec![
                Header::new(HEADER_TYPE, TYPE_EVENT),
                Header::new(HEADER_SUM, "3"),
                Header::new(HEADER_SEQ, "1"),
                Header::new(HEADER_MESSAGE_ID, "om_abc"),
            ],
            ..Frame::default()
        };
        assert_eq!(f.msg_type(), Some(TYPE_EVENT));
        assert_eq!(f.header_int(HEADER_SUM), Some(3));
        assert_eq!(f.header_int(HEADER_SEQ), Some(1));
        assert_eq!(f.message_id(), Some("om_abc"));
        assert_eq!(f.header("absent"), None);
        assert_eq!(f.header_int("absent"), None);
    }

    #[test]
    fn ping_frame_is_a_control_ping() {
        let p = Frame::ping(42);
        assert_eq!(p.method, METHOD_CONTROL);
        assert_eq!(p.service, 42);
        assert_eq!(p.msg_type(), Some(TYPE_PING));
        assert!(p.payload.is_empty());
    }

    #[test]
    fn ack_echoes_headers_and_sets_response_payload() {
        let recv = Frame {
            method: METHOD_DATA,
            service: 5,
            headers: vec![
                Header::new(HEADER_TYPE, TYPE_EVENT),
                Header::new(HEADER_MESSAGE_ID, "om_xyz"),
            ],
            payload: b"event-body".to_vec(),
            ..Frame::default()
        };
        let ack = recv.into_ack(ACK_OK, 12);
        assert_eq!(ack.method, METHOD_DATA);
        assert_eq!(ack.service, 5);
        assert_eq!(ack.message_id(), Some("om_xyz"));
        assert_eq!(ack.header_int(HEADER_BIZ_RT), Some(12));
        assert_eq!(ack.payload, b"{\"code\":200}".to_vec());
        assert!(ack.payload_encoding.is_empty());
    }
}
