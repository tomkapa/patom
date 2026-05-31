//! Bounded newtype wrapping the cookie `Domain` attribute the operator
//! configures via `PATOM_COOKIE_DOMAIN` (e.g. `.patom.app`).
//!
//! Per CLAUDE.md §1: the value crosses into the typed world exactly once
//! here, via [`CookieDomain::try_from`]. Setting `Domain` on the session
//! and CSRF cookies makes them visible across `patom.app` and
//! `app.patom.app` (same registrable domain, so `SameSite=Lax` still
//! applies), so the marketing apex can read the logged-in state. A
//! malformed value reaching `Set-Cookie` would silently drop the
//! attribute (browsers ignore an invalid `Domain`) and break the shared
//! session, so we validate strictly at the boundary instead.
//!
//! Accepts a dotted, lowercase DNS name with an optional single leading
//! dot — the cross-subdomain idiom. Rejects scheme, path, port,
//! userinfo, whitespace, uppercase, and anything over
//! [`COOKIE_DOMAIN_MAX_LEN`].

use std::sync::Arc;

use crate::types::ParseError;

use super::limits::COOKIE_DOMAIN_MAX_LEN;

/// Maximum bytes of a single DNS label (RFC 1035 §2.3.4).
const LABEL_MAX_LEN: usize = 63;

/// Validated cookie `Domain` attribute.
///
/// Construction goes through [`CookieDomain::try_from`] only. No `pub`
/// inner field; consumers read via [`CookieDomain::as_str`].
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct CookieDomain(Arc<str>);

impl CookieDomain {
    pub const MAX_BYTES: usize = COOKIE_DOMAIN_MAX_LEN;

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for CookieDomain {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("CookieDomain").field(&self.as_str()).finish()
    }
}

impl std::fmt::Display for CookieDomain {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Validate a dotted DNS name (already stripped of any single leading
/// dot). Returns a `&'static str` detail on failure for
/// [`ParseError::Malformed`].
fn validate_dns_name(name: &str) -> Result<(), &'static str> {
    if name.is_empty() {
        return Err("empty domain");
    }
    if !name.contains('.') {
        return Err("must contain a dot");
    }
    for label in name.split('.') {
        if label.is_empty() {
            return Err("empty label (leading, trailing, or doubled dot)");
        }
        if label.len() > LABEL_MAX_LEN {
            return Err("label exceeds 63 bytes");
        }
        if label.starts_with('-') || label.ends_with('-') {
            return Err("label has a leading or trailing hyphen");
        }
        let ascii_dns = label
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
        if !ascii_dns {
            return Err("label has a non [a-z0-9-] character (uppercase/scheme/port/path?)");
        }
    }
    Ok(())
}

impl TryFrom<&str> for CookieDomain {
    type Error = ParseError;

    fn try_from(raw: &str) -> Result<Self, Self::Error> {
        if raw.is_empty() {
            return Err(ParseError::Empty {
                field: "cookie_domain",
            });
        }
        if raw.len() > Self::MAX_BYTES {
            return Err(ParseError::TooLong {
                field: "cookie_domain",
                max: Self::MAX_BYTES,
                got: raw.len(),
            });
        }
        // A single leading dot is the legacy cross-subdomain form and is
        // allowed; strip it before per-label validation.
        let name = raw.strip_prefix('.').unwrap_or(raw);
        validate_dns_name(name).map_err(|detail| ParseError::Malformed {
            field: "cookie_domain",
            detail,
        })?;
        Ok(Self(Arc::from(raw)))
    }
}

impl TryFrom<String> for CookieDomain {
    type Error = ParseError;
    fn try_from(raw: String) -> Result<Self, Self::Error> {
        Self::try_from(raw.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_apex_and_leading_dot_forms() {
        for raw in [".patom.app", "patom.app", "app.patom.app", "a.b.co"] {
            let d = CookieDomain::try_from(raw).expect("valid domain");
            assert_eq!(d.as_str(), raw);
            assert!(d.as_str().contains('.'));
        }
    }

    #[test]
    fn rejects_empty() {
        let err = CookieDomain::try_from("").expect_err("rejected");
        assert!(matches!(
            err,
            ParseError::Empty {
                field: "cookie_domain"
            }
        ));
    }

    #[test]
    fn rejects_scheme_and_path_and_port() {
        for raw in ["https://patom.app", "patom.app/x", "patom.app:443"] {
            let err = CookieDomain::try_from(raw).expect_err("rejected");
            assert!(matches!(
                err,
                ParseError::Malformed {
                    field: "cookie_domain",
                    ..
                }
            ));
        }
    }

    #[test]
    fn rejects_uppercase_and_whitespace_and_no_dot() {
        for raw in ["Patom.app", " patom.app", "localhost"] {
            let err = CookieDomain::try_from(raw).expect_err("rejected");
            assert!(matches!(
                err,
                ParseError::Malformed {
                    field: "cookie_domain",
                    ..
                }
            ));
        }
    }

    #[test]
    fn rejects_doubled_and_trailing_dot() {
        for raw in ["patom..app", "patom.app.", ".", "-bad.app", "bad-.app"] {
            let err = CookieDomain::try_from(raw).expect_err("rejected");
            assert!(matches!(
                err,
                ParseError::Malformed {
                    field: "cookie_domain",
                    ..
                }
            ));
        }
    }

    #[test]
    fn rejects_oversize() {
        let oversized = format!("{}.app", "a".repeat(CookieDomain::MAX_BYTES));
        let err = CookieDomain::try_from(oversized.as_str()).expect_err("rejected");
        assert!(matches!(
            err,
            ParseError::TooLong {
                field: "cookie_domain",
                ..
            }
        ));
    }
}
