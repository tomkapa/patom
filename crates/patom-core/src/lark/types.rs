//! Domain newtypes for the Lark adapter (CLAUDE.md §1 — parse, don't validate).
//!
//! Every value that crosses into the Lark module funnels through one of these
//! `TryFrom` constructors. The boundaries are the WS frame decoder
//! (`bridge.rs`), the handshake/token HTTP responses (`handshake.rs`/`token.rs`),
//! and the admin registration route (`admin_routes.rs`). Downstream code never
//! reconstructs IDs from raw strings.
//!
//! Like the Slack adapter, the newtypes are conservative about Lark's actual ID
//! grammar: we enforce length + a restricted ASCII charset + non-empty. The
//! exact prefixes (`cli_`/`cli_a…` app ids, `ou_…` open ids, `on_…` union ids,
//! `oc_…` chats, `om_…` messages, `omt_…` threads) are allowed to drift — what
//! matters is that nobody can sneak a multi-kilobyte string or a control
//! character into the bridge by lying about a frame payload.

use std::fmt;
use std::sync::Arc;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::types::{ParseError, SecretString};

/// Maximum length (bytes) for any Lark-side opaque identifier. Lark `open_id`s
/// and `union_id`s run ~30 bytes; `user_id` (employee_id) is tenant-defined.
/// 128 is generous head-room without inviting abuse.
pub const LARK_ID_MAX_LEN: usize = 128;

/// Maximum length for a Lark `app_secret`. Secrets are ~32 bytes; cap at 256.
pub const LARK_SECRET_MAX_LEN: usize = 256;

// ─────────────────────────────────────────────────────────────────────────
// Opaque identifiers
// ─────────────────────────────────────────────────────────────────────────

/// Emit a `Clone + PartialEq + Eq + Hash` newtype around `Arc<str>` with
/// `TryFrom<&str>` / `TryFrom<String>` funnelled through [`parse_lark_id`],
/// plus the `Debug` / `Display` / `Serialize` / `Deserialize` glue.
macro_rules! lark_string_newtype {
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
                parse_lark_id(raw, $field).map(|s| Self(Arc::from(s)))
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

lark_string_newtype! {
    /// A self-built Lark app identifier (`cli_…`). One app == one bot == one agent.
    LarkAppId, "lark_app_id"
}

lark_string_newtype! {
    /// A Lark tenant key — the tenant-scoped identity namespace (`scope_id`).
    TenantKey, "lark_tenant_key"
}

lark_string_newtype! {
    /// A Lark `user_id` (== tenant "employee_id"). The stable identity key.
    LarkUserId, "lark_user_id"
}

lark_string_newtype! {
    /// A Lark `open_id` (`ou_…`) — per-app satellite, the `@`-tag handle.
    LarkOpenId, "lark_open_id"
}

lark_string_newtype! {
    /// A Lark `union_id` (`on_…`) — service-provider-scoped id (carried, not keyed).
    LarkUnionId, "lark_union_id"
}

lark_string_newtype! {
    /// A Lark chat identifier (`oc_…`).
    LarkChatId, "lark_chat_id"
}

lark_string_newtype! {
    /// A Lark message identifier (`om_…`).
    LarkMessageId, "lark_message_id"
}

lark_string_newtype! {
    /// A Lark reply-thread anchor (`omt_…` / the root message id).
    LarkThreadId, "lark_thread_id"
}

lark_string_newtype! {
    /// A Lark event identifier (`header.event_id`) — the live-event dedup key.
    LarkEventId, "lark_event_id"
}

lark_string_newtype! {
    /// A Lark message-resource key — an `image_key` (`img_v3_…`) or `file_key`
    /// (`file_v3_…`) carried by an inbound message and used as the path param to
    /// the resource-download endpoint (issue #187).
    LarkFileKey, "lark_file_key"
}

// ─────────────────────────────────────────────────────────────────────────
// Secrets
// ─────────────────────────────────────────────────────────────────────────

/// A self-built app's `app_secret`. `Debug`/`Display` redact; the bytes are
/// only available through [`LarkAppSecret::expose`].
#[derive(Clone)]
pub struct LarkAppSecret(SecretString);

impl LarkAppSecret {
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

impl TryFrom<String> for LarkAppSecret {
    type Error = ParseError;

    fn try_from(raw: String) -> Result<Self, Self::Error> {
        if raw.is_empty() {
            return Err(ParseError::Empty {
                field: "lark_app_secret",
            });
        }
        if raw.len() > LARK_SECRET_MAX_LEN {
            return Err(ParseError::TooLong {
                field: "lark_app_secret",
                max: LARK_SECRET_MAX_LEN,
                got: raw.len(),
            });
        }
        Ok(Self(SecretString::try_from(raw).map_err(|_| {
            ParseError::Empty {
                field: "lark_app_secret",
            }
        })?))
    }
}

impl fmt::Debug for LarkAppSecret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("LarkAppSecret(***)")
    }
}

/// An app's **Encrypt Key** — signs a `card.action.trigger` callback (issue #214).
///
/// The per-app secret Lark mixes into the request signature. `Debug` redacts; the
/// bytes are only available through [`LarkEncryptKey::expose`].
#[derive(Clone)]
pub struct LarkEncryptKey(SecretString);

impl LarkEncryptKey {
    #[must_use]
    pub fn expose(&self) -> &str {
        self.0.expose()
    }
}

impl TryFrom<String> for LarkEncryptKey {
    type Error = ParseError;

    fn try_from(raw: String) -> Result<Self, Self::Error> {
        if raw.is_empty() {
            return Err(ParseError::Empty {
                field: "lark_encrypt_key",
            });
        }
        if raw.len() > LARK_SECRET_MAX_LEN {
            return Err(ParseError::TooLong {
                field: "lark_encrypt_key",
                max: LARK_SECRET_MAX_LEN,
                got: raw.len(),
            });
        }
        Ok(Self(SecretString::try_from(raw).map_err(|_| {
            ParseError::Empty {
                field: "lark_encrypt_key",
            }
        })?))
    }
}

impl fmt::Debug for LarkEncryptKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("LarkEncryptKey(***)")
    }
}

/// An app's **Verification Token** — echoed in the callback body (issue #214).
///
/// The per-app secret in `header.token` that the route checks constant-time.
/// `Debug` redacts; the bytes are only available through
/// [`LarkVerificationToken::expose`].
#[derive(Clone)]
pub struct LarkVerificationToken(SecretString);

impl LarkVerificationToken {
    #[must_use]
    pub fn expose(&self) -> &str {
        self.0.expose()
    }
}

impl TryFrom<String> for LarkVerificationToken {
    type Error = ParseError;

    fn try_from(raw: String) -> Result<Self, Self::Error> {
        if raw.is_empty() {
            return Err(ParseError::Empty {
                field: "lark_verification_token",
            });
        }
        if raw.len() > LARK_SECRET_MAX_LEN {
            return Err(ParseError::TooLong {
                field: "lark_verification_token",
                max: LARK_SECRET_MAX_LEN,
                got: raw.len(),
            });
        }
        Ok(Self(SecretString::try_from(raw).map_err(|_| {
            ParseError::Empty {
                field: "lark_verification_token",
            }
        })?))
    }
}

impl fmt::Debug for LarkVerificationToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("LarkVerificationToken(***)")
    }
}

// ─────────────────────────────────────────────────────────────────────────
// Shared parsing helper
// ─────────────────────────────────────────────────────────────────────────

/// Length + restricted-ASCII gate for every Lark opaque id. Lark ids are
/// `[A-Za-z0-9_-]`; reject spaces / control chars / anything else so a hostile
/// frame can't smuggle markup or a SQL fragment past the boundary.
fn parse_lark_id<'a>(raw: &'a str, field: &'static str) -> Result<&'a str, ParseError> {
    if raw.is_empty() {
        return Err(ParseError::Empty { field });
    }
    if raw.len() > LARK_ID_MAX_LEN {
        return Err(ParseError::TooLong {
            field,
            max: LARK_ID_MAX_LEN,
            got: raw.len(),
        });
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_id_accepts_typical() {
        let a = LarkAppId::try_from("cli_a1b2c3d4e5f6").expect("valid app id");
        assert_eq!(a.as_str(), "cli_a1b2c3d4e5f6");
    }

    #[test]
    fn ids_reject_empty_too_long_and_non_alnum() {
        assert!(LarkUserId::try_from("").is_err());
        let big = "a".repeat(LARK_ID_MAX_LEN + 1);
        assert!(LarkUserId::try_from(big.as_str()).is_err());
        assert!(LarkOpenId::try_from("ou_ x").is_err());
        assert!(LarkChatId::try_from("oc_;DROP").is_err());
    }

    #[test]
    fn open_id_serde_roundtrip() {
        let o = LarkOpenId::try_from("ou_abc123").expect("valid");
        let json = serde_json::to_string(&o).expect("ser");
        assert_eq!(json, "\"ou_abc123\"");
        let back: LarkOpenId = serde_json::from_str(&json).expect("de");
        assert_eq!(back, o);
    }

    #[test]
    fn user_id_serde_rejects_bad_string() {
        let r: Result<LarkUserId, _> = serde_json::from_str("\"bad user!\"");
        assert!(r.is_err());
    }

    #[test]
    fn app_secret_redacts_debug() {
        let s = LarkAppSecret::try_from("super-secret-value".to_owned()).expect("valid");
        let dbg = format!("{s:?}");
        assert!(!dbg.contains("super-secret"));
        assert_eq!(dbg, "LarkAppSecret(***)");
        assert_eq!(s.expose(), "super-secret-value");
        assert!(!s.is_empty());
    }

    #[test]
    fn app_secret_rejects_empty_and_too_long() {
        assert!(LarkAppSecret::try_from(String::new()).is_err());
        let big = "a".repeat(LARK_SECRET_MAX_LEN + 1);
        assert!(LarkAppSecret::try_from(big).is_err());
    }
}
