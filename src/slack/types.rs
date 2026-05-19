//! Domain newtypes for the Slack adapter (CLAUDE.md §1 — parse, don't validate).
//!
//! Every value that crosses into the Slack module funnels through one of these
//! `TryFrom` constructors. The boundaries are the webhook handler
//! (`events.rs`), the OAuth callback (`oauth.rs`), and the workspace store
//! (`workspace.rs`). Downstream code never reconstructs IDs from raw strings.
//!
//! The newtypes are deliberately conservative about Slack's actual ID grammar:
//! we only enforce length + ASCII + non-empty. The full set of valid prefixes
//! (`T*` / `E*` for teams, `U*` / `W*` for users, `C*` / `G*` / `D*` for
//! channels) is allowed to drift; what we care about is that nobody can sneak
//! a multi-kilobyte string or a UTF-8 control character into the bridge by
//! lying about the event payload.

use std::fmt;
use std::sync::Arc;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::types::{ParseError, SecretString};

/// Maximum length (bytes) for any Slack-side opaque identifier. The longest
/// Slack ID we've seen in the wild is a 21-byte Enterprise Grid user (`W…`);
/// 32 is a generous head-room cap.
pub const SLACK_ID_MAX_LEN: usize = 32;

/// Maximum length for a Slack message `ts` / `thread_ts`. Slack's wire shape
/// is `digits.digits` (Unix seconds then microseconds); 32 bytes accommodates
/// any conceivable future precision bump.
pub const SLACK_TS_MAX_LEN: usize = 32;

/// Maximum length for an `xoxb-` bot token. Slack tokens are typically
/// ~72 bytes; cap at 256 to stay well above realistic growth.
pub const SLACK_TOKEN_MAX_LEN: usize = 256;

/// Length of an HMAC-SHA256 signature in hex (64 chars) plus the `v0=` prefix.
pub const SLACK_SIGNATURE_LEN: usize = 67;

// ─────────────────────────────────────────────────────────────────────────
// Opaque identifiers
// ─────────────────────────────────────────────────────────────────────────

/// Emit a `Clone + PartialEq + Eq + Hash`-derived newtype around `Arc<str>`
/// with `TryFrom<&str>` / `TryFrom<String>` funnelled through `$parser`,
/// plus the `Debug` / `Display` / `Serialize` / `Deserialize` glue every
/// Slack wire type needs.
macro_rules! slack_string_newtype {
    (
        $(#[$meta:meta])*
        $name:ident, $field:literal, $parser:path
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
                $parser(raw, $field).map(|s| Self(Arc::from(s)))
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

slack_string_newtype! {
    /// A Slack workspace identifier (`T…` or Enterprise Grid `E…`).
    SlackTeamId, "slack_team_id", parse_slack_id
}

slack_string_newtype! {
    /// A Slack channel / group / DM identifier (`C…` / `G…` / `D…`).
    SlackChannelId, "slack_channel_id", parse_slack_id
}

slack_string_newtype! {
    /// A Slack user identifier (`U…` or Enterprise Grid `W…`).
    SlackUserId, "slack_user_id", parse_slack_id
}

// `ts` and `thread_ts` share the same wire format but kept as separate
// types so a `chat.postMessage` call cannot accidentally swap them.
slack_string_newtype! {
    /// A Slack message timestamp (`<unix_seconds>.<microseconds>`).
    SlackTs, "slack_ts", parse_slack_ts
}

slack_string_newtype! {
    /// A Slack thread anchor (`<unix_seconds>.<microseconds>`).
    SlackThreadTs, "slack_thread_ts", parse_slack_ts
}

// ─────────────────────────────────────────────────────────────────────────
// Secrets
// ─────────────────────────────────────────────────────────────────────────

/// A `xoxb-…` bot token. `Debug` and `Display` redact; the bytes are only
/// available through [`SlackBotToken::expose`].
#[derive(Clone)]
pub struct SlackBotToken(SecretString);

impl SlackBotToken {
    #[must_use]
    pub fn expose(&self) -> &str {
        self.0.expose()
    }

    /// Length in bytes — useful for assertions; reveals nothing.
    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl TryFrom<String> for SlackBotToken {
    type Error = ParseError;

    fn try_from(raw: String) -> Result<Self, Self::Error> {
        if raw.is_empty() {
            return Err(ParseError::Empty {
                field: "slack_bot_token",
            });
        }
        if raw.len() > SLACK_TOKEN_MAX_LEN {
            return Err(ParseError::TooLong {
                field: "slack_bot_token",
                max: SLACK_TOKEN_MAX_LEN,
                got: raw.len(),
            });
        }
        if !raw.starts_with("xoxb-") {
            return Err(ParseError::Malformed {
                field: "slack_bot_token",
                detail: "must start with xoxb-",
            });
        }
        Ok(Self(SecretString::try_from(raw).map_err(|_| {
            ParseError::Malformed {
                field: "slack_bot_token",
                detail: "empty after prefix",
            }
        })?))
    }
}

impl fmt::Debug for SlackBotToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SlackBotToken(***)")
    }
}

/// A `v0=<hex64>` HMAC-SHA256 signature header value. `Debug` is opaque
/// so accidental log calls never surface the literal signature (logs from
/// a flaky proxy are a real source of accidental disclosure).
#[derive(Clone)]
pub struct SlackSignature(Arc<str>);

impl SlackSignature {
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<&str> for SlackSignature {
    type Error = ParseError;

    fn try_from(raw: &str) -> Result<Self, Self::Error> {
        if raw.len() != SLACK_SIGNATURE_LEN {
            return Err(ParseError::Malformed {
                field: "slack_signature",
                detail: "expected length 67 (v0= + 64 hex)",
            });
        }
        if !raw.starts_with("v0=") {
            return Err(ParseError::Malformed {
                field: "slack_signature",
                detail: "missing v0= prefix",
            });
        }
        let hex_tail = &raw[3..];
        if !hex_tail.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(ParseError::Malformed {
                field: "slack_signature",
                detail: "non-hex digits after v0=",
            });
        }
        Ok(Self(Arc::from(raw)))
    }
}

impl TryFrom<String> for SlackSignature {
    type Error = ParseError;
    fn try_from(raw: String) -> Result<Self, Self::Error> {
        Self::try_from(raw.as_str())
    }
}

impl fmt::Debug for SlackSignature {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SlackSignature(***)")
    }
}

// ─────────────────────────────────────────────────────────────────────────
// Webhook timestamp header
// ─────────────────────────────────────────────────────────────────────────

/// The Unix-epoch-seconds value carried in `X-Slack-Request-Timestamp`.
/// Range-checked at parse time to keep downstream arithmetic
/// (skew comparison against `now()`) safe.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct SlackEventTimestamp(i64);

impl SlackEventTimestamp {
    /// Sanity upper bound — year ~2200, well past anything Slack would send.
    /// Keeps `(now - ts)` arithmetic away from `i64::MAX`.
    pub const MAX: i64 = 7_258_118_400; // 2200-01-01 UTC

    #[must_use]
    pub fn get(self) -> i64 {
        self.0
    }

    /// Construct from a known-valid epoch value. Used by tests and by
    /// internal call sites that have already validated the range; the
    /// boundary path is `TryFrom<&str>`.
    #[must_use]
    pub const fn from_epoch_secs(secs: i64) -> Option<Self> {
        if secs > 0 && secs < Self::MAX {
            Some(Self(secs))
        } else {
            None
        }
    }
}

impl TryFrom<&str> for SlackEventTimestamp {
    type Error = ParseError;

    fn try_from(raw: &str) -> Result<Self, Self::Error> {
        let parsed: i64 = raw.parse().map_err(|_| ParseError::Malformed {
            field: "slack_event_timestamp",
            detail: "expected base-10 i64",
        })?;
        Self::from_epoch_secs(parsed).ok_or(ParseError::OutOfRange {
            field: "slack_event_timestamp",
            detail: "must be (0, year-2200)",
        })
    }
}

// ─────────────────────────────────────────────────────────────────────────
// Shared parsing helpers
// ─────────────────────────────────────────────────────────────────────────

fn parse_slack_id<'a>(raw: &'a str, field: &'static str) -> Result<&'a str, ParseError> {
    if raw.is_empty() {
        return Err(ParseError::Empty { field });
    }
    if raw.len() > SLACK_ID_MAX_LEN {
        return Err(ParseError::TooLong {
            field,
            max: SLACK_ID_MAX_LEN,
            got: raw.len(),
        });
    }
    // Slack IDs are ASCII alphanumeric (sometimes including `-` for
    // Enterprise resources, but never spaces / control chars). Reject
    // anything outside `[A-Za-z0-9_-]`.
    if !raw
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return Err(ParseError::Malformed {
            field,
            detail: "non-alphanumeric character",
        });
    }
    Ok(raw)
}

fn parse_slack_ts<'a>(raw: &'a str, field: &'static str) -> Result<&'a str, ParseError> {
    if raw.is_empty() {
        return Err(ParseError::Empty { field });
    }
    if raw.len() > SLACK_TS_MAX_LEN {
        return Err(ParseError::TooLong {
            field,
            max: SLACK_TS_MAX_LEN,
            got: raw.len(),
        });
    }
    // Wire form is `digits.digits` — exactly one dot, digits both sides.
    let mut parts = raw.split('.');
    let lhs = parts.next().ok_or(ParseError::Malformed {
        field,
        detail: "expected <secs>.<micros>",
    })?;
    let rhs = parts.next().ok_or(ParseError::Malformed {
        field,
        detail: "missing fractional part",
    })?;
    if parts.next().is_some() {
        return Err(ParseError::Malformed {
            field,
            detail: "more than one '.'",
        });
    }
    if lhs.is_empty() || rhs.is_empty() {
        return Err(ParseError::Malformed {
            field,
            detail: "empty section around '.'",
        });
    }
    if !lhs.bytes().all(|b| b.is_ascii_digit()) || !rhs.bytes().all(|b| b.is_ascii_digit()) {
        return Err(ParseError::Malformed {
            field,
            detail: "non-digit character",
        });
    }
    Ok(raw)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ───── SlackTeamId / SlackChannelId / SlackUserId share parse_slack_id ─────

    #[test]
    fn slack_team_id_accepts_typical() {
        let t = SlackTeamId::try_from("T012ABCDE").expect("valid team id");
        assert_eq!(t.as_str(), "T012ABCDE");
    }

    #[test]
    fn slack_team_id_rejects_empty_too_long_and_non_alnum() {
        assert!(SlackTeamId::try_from("").is_err());
        let big = "A".repeat(SLACK_ID_MAX_LEN + 1);
        assert!(SlackTeamId::try_from(big.as_str()).is_err());
        assert!(SlackTeamId::try_from("T 12 ABC").is_err());
        assert!(SlackTeamId::try_from("T;DROP TABLE").is_err());
    }

    #[test]
    fn slack_channel_id_serde_roundtrip() {
        let c = SlackChannelId::try_from("C0XYZ123").expect("valid");
        let json = serde_json::to_string(&c).expect("ser");
        assert_eq!(json, "\"C0XYZ123\"");
        let back: SlackChannelId = serde_json::from_str(&json).expect("de");
        assert_eq!(back, c);
    }

    #[test]
    fn slack_user_id_serde_rejects_bad_string() {
        let r: Result<SlackUserId, _> = serde_json::from_str("\"bad user!\"");
        assert!(r.is_err());
    }

    // ───── SlackTs / SlackThreadTs share parse_slack_ts ─────

    #[test]
    fn slack_ts_accepts_canonical() {
        let t = SlackTs::try_from("1234567890.123456").expect("valid");
        assert_eq!(t.as_str(), "1234567890.123456");
    }

    #[test]
    fn slack_ts_rejects_malformed() {
        assert!(SlackTs::try_from("").is_err());
        assert!(SlackTs::try_from("1234567890").is_err()); // no dot
        assert!(SlackTs::try_from(".123456").is_err()); // empty lhs
        assert!(SlackTs::try_from("1234567890.").is_err()); // empty rhs
        assert!(SlackTs::try_from("1234.56.78").is_err()); // two dots
        assert!(SlackTs::try_from("abc.123").is_err()); // non-digit
    }

    #[test]
    fn slack_thread_ts_parses_independently_from_ts() {
        let t1 = SlackThreadTs::try_from("1700000000.000100").expect("valid");
        let t2 = SlackTs::try_from("1700000000.000100").expect("valid");
        // They are distinct types — assertion is at compile time; this
        // test exists to document the design choice.
        assert_eq!(t1.as_str(), t2.as_str());
    }

    // ───── SlackBotToken ─────

    #[test]
    fn slack_bot_token_requires_xoxb_prefix() {
        assert!(SlackBotToken::try_from("xoxb-12345".to_string()).is_ok());
        assert!(SlackBotToken::try_from("xoxp-12345".to_string()).is_err());
        assert!(SlackBotToken::try_from(String::new()).is_err());
    }

    #[test]
    fn slack_bot_token_redacts_debug() {
        let t = SlackBotToken::try_from("xoxb-very-secret".to_string()).expect("valid");
        let s = format!("{t:?}");
        assert!(!s.contains("very-secret"));
        assert_eq!(s, "SlackBotToken(***)");
    }

    #[test]
    fn slack_bot_token_exposes_bytes_via_method() {
        let t = SlackBotToken::try_from("xoxb-abc".to_string()).expect("valid");
        assert_eq!(t.expose(), "xoxb-abc");
        assert_eq!(t.len(), "xoxb-abc".len());
        assert!(!t.is_empty());
    }

    // ───── SlackSignature ─────

    #[test]
    fn slack_signature_requires_v0_prefix_and_64_hex() {
        let valid = format!("v0={}", "a".repeat(64));
        assert!(SlackSignature::try_from(valid.as_str()).is_ok());

        // Wrong length
        assert!(SlackSignature::try_from(format!("v0={}", "a".repeat(63))).is_err());
        // Missing prefix
        assert!(SlackSignature::try_from("a".repeat(67)).is_err());
        // Non-hex in tail
        let bad = format!("v0={}", "z".repeat(64));
        assert!(SlackSignature::try_from(bad.as_str()).is_err());
    }

    #[test]
    fn slack_signature_redacts_debug() {
        let s =
            SlackSignature::try_from(format!("v0={}", "a".repeat(64)).as_str()).expect("valid sig");
        let dbg = format!("{s:?}");
        assert!(!dbg.contains("aaaa"));
        assert_eq!(dbg, "SlackSignature(***)");
    }

    // ───── SlackEventTimestamp ─────

    #[test]
    fn slack_event_timestamp_parses_positive_in_range() {
        let t = SlackEventTimestamp::try_from("1700000000").expect("valid");
        assert_eq!(t.get(), 1_700_000_000);
    }

    #[test]
    fn slack_event_timestamp_rejects_zero_negative_and_far_future() {
        assert!(SlackEventTimestamp::try_from("0").is_err());
        assert!(SlackEventTimestamp::try_from("-1").is_err());
        let far_future = SlackEventTimestamp::MAX + 1;
        assert!(SlackEventTimestamp::try_from(far_future.to_string().as_str()).is_err());
    }

    #[test]
    fn slack_event_timestamp_rejects_non_numeric() {
        assert!(SlackEventTimestamp::try_from("abc").is_err());
        assert!(SlackEventTimestamp::try_from("17.0").is_err());
    }
}
