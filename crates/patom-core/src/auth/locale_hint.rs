//! Bounded newtype wrapping a raw locale tag the system received from an
//! external source (Google's `userinfo.locale` or an inbound
//! `Accept-Language` primary tag).
//!
//! Per CLAUDE.md §1: external input is wrapped in a typed newtype at the
//! boundary so callers cannot pass an unbounded string across module
//! lines. The single invariant is the byte-length cap shared with the
//! `oauth_login_states.detected_locale` column CHECK — see
//! [`super::limits::DETECTED_LOCALE_MAX_LEN`]. We do **not** assert
//! BCP-47 shape here: the hint is fed to
//! [`super::Language::from_locale_hint`] which already tolerates unknown
//! / malformed values by falling through to [`super::Language::DEFAULT`].
//!
//! The wire body stays an opaque string (the parser is the final
//! filter); the newtype's only job is to make "this is a bounded
//! external hint, not free-form text" visible at every signature.

use std::sync::Arc;

use crate::types::ParseError;

use super::limits::DETECTED_LOCALE_MAX_LEN;

/// External locale tag bounded by [`DETECTED_LOCALE_MAX_LEN`].
///
/// Construction goes through [`LocaleHint::try_from`] only. No `pub`
/// inner field; consumers read via [`LocaleHint::as_str`].
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct LocaleHint(Arc<str>);

impl LocaleHint {
    pub const MAX_BYTES: usize = DETECTED_LOCALE_MAX_LEN;

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for LocaleHint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("LocaleHint").field(&self.as_str()).finish()
    }
}

impl std::fmt::Display for LocaleHint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl TryFrom<&str> for LocaleHint {
    type Error = ParseError;

    fn try_from(raw: &str) -> Result<Self, Self::Error> {
        if raw.is_empty() {
            return Err(ParseError::Empty {
                field: "locale_hint",
            });
        }
        if raw.len() > Self::MAX_BYTES {
            return Err(ParseError::TooLong {
                field: "locale_hint",
                max: Self::MAX_BYTES,
                got: raw.len(),
            });
        }
        Ok(Self(Arc::from(raw)))
    }
}

impl TryFrom<String> for LocaleHint {
    type Error = ParseError;
    fn try_from(raw: String) -> Result<Self, Self::Error> {
        Self::try_from(raw.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_short_tags() {
        for raw in ["vi", "en", "en-US", "zh-Hant"] {
            let hint = LocaleHint::try_from(raw).expect("valid hint");
            assert_eq!(hint.as_str(), raw);
        }
    }

    #[test]
    fn rejects_empty() {
        let err = LocaleHint::try_from("").expect_err("rejected");
        assert!(matches!(
            err,
            ParseError::Empty {
                field: "locale_hint"
            }
        ));
    }

    #[test]
    fn rejects_oversize() {
        let oversized = "a".repeat(LocaleHint::MAX_BYTES + 1);
        let err = LocaleHint::try_from(oversized.as_str()).expect_err("rejected");
        assert!(matches!(
            err,
            ParseError::TooLong {
                field: "locale_hint",
                ..
            }
        ));
    }
}
