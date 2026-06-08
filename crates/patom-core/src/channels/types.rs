//! Channel identity newtypes. Per CLAUDE.md §1, every value carrying an
//! invariant is a newtype reached through a `TryFrom` smart constructor.

use std::sync::Arc;

use super::limits::CHANNEL_NAME_MAX_LEN;
use crate::types::ParseError;

crate::uuid_newtype! {
    /// Opaque identifier for a `channels` row.
    pub ChannelId
}

/// URL-safe channel name. Mirrors the migration CHECK regex
/// `^[a-z0-9][a-z0-9-]{0,62}$`: lowercase ASCII, digits and hyphens, first
/// char alphanumeric, at most 63 bytes.
///
/// Input is trimmed and lowercased before validation, so `"Eng-Team"` parses to
/// `eng-team`; anything still outside the charset (spaces, underscores, …) is
/// rejected rather than silently rewritten.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct ChannelName(Arc<str>);

impl ChannelName {
    /// Single source of truth lives in [`super::limits`]; aliased here so the
    /// DB CHECK, the limits constant, and this newtype cannot drift (§5).
    pub const MAX_BYTES: usize = CHANNEL_NAME_MAX_LEN;

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for ChannelName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("ChannelName").field(&self.as_str()).finish()
    }
}

impl std::fmt::Display for ChannelName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl TryFrom<&str> for ChannelName {
    type Error = ParseError;

    fn try_from(raw: &str) -> Result<Self, Self::Error> {
        let normalized = raw.trim().to_ascii_lowercase();
        if normalized.is_empty() {
            return Err(ParseError::Empty {
                field: "channel_name",
            });
        }
        if normalized.len() > Self::MAX_BYTES {
            return Err(ParseError::TooLong {
                field: "channel_name",
                max: Self::MAX_BYTES,
                got: normalized.len(),
            });
        }
        let mut chars = normalized.chars();
        let first = chars.next().ok_or(ParseError::Empty {
            field: "channel_name",
        })?;
        if !first.is_ascii_lowercase() && !first.is_ascii_digit() {
            return Err(ParseError::Malformed {
                field: "channel_name",
                detail: "must start with [a-z0-9]",
            });
        }
        for ch in chars {
            let ok = ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '-';
            if !ok {
                return Err(ParseError::Malformed {
                    field: "channel_name",
                    detail: "only [a-z0-9-] after the first char",
                });
            }
        }
        Ok(Self(Arc::from(normalized.as_str())))
    }
}

impl TryFrom<String> for ChannelName {
    type Error = ParseError;
    fn try_from(raw: String) -> Result<Self, Self::Error> {
        Self::try_from(raw.as_str())
    }
}

impl serde::Serialize for ChannelName {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_plain_lowercase_name() {
        let name = ChannelName::try_from("general").expect("valid");
        assert_eq!(name.as_str(), "general");
    }

    #[test]
    fn accepts_hyphens_and_digits() {
        let name = ChannelName::try_from("eng-team-2").expect("valid");
        assert_eq!(name.as_str(), "eng-team-2");
    }

    #[test]
    fn lowercases_and_trims_input() {
        let name = ChannelName::try_from("  Eng-Team  ").expect("valid");
        assert_eq!(name.as_str(), "eng-team");
    }

    #[test]
    fn accepts_max_length() {
        let raw = "a".repeat(ChannelName::MAX_BYTES);
        let name = ChannelName::try_from(raw.as_str()).expect("valid at cap");
        assert_eq!(name.as_str().len(), ChannelName::MAX_BYTES);
    }

    #[test]
    fn rejects_empty() {
        let err = ChannelName::try_from("   ").expect_err("empty");
        assert!(matches!(
            err,
            ParseError::Empty {
                field: "channel_name"
            }
        ));
    }

    #[test]
    fn rejects_too_long() {
        let raw = "a".repeat(ChannelName::MAX_BYTES + 1);
        let err = ChannelName::try_from(raw.as_str()).expect_err("too long");
        assert!(matches!(
            err,
            ParseError::TooLong {
                field: "channel_name",
                ..
            }
        ));
    }

    #[test]
    fn rejects_leading_hyphen() {
        let err = ChannelName::try_from("-general").expect_err("bad first char");
        assert!(matches!(
            err,
            ParseError::Malformed {
                field: "channel_name",
                ..
            }
        ));
    }

    #[test]
    fn rejects_spaces_and_underscores() {
        for bad in ["my channel", "eng_team", "a.b", "café"] {
            let err = ChannelName::try_from(bad).expect_err("bad charset");
            assert!(
                matches!(
                    err,
                    ParseError::Malformed {
                        field: "channel_name",
                        ..
                    }
                ),
                "expected malformed for {bad:?}"
            );
        }
    }
}
