//! Identity-surface newtypes. Per CLAUDE.md §1 every value carrying an
//! invariant is a newtype with a `TryFrom` smart constructor.

use std::sync::Arc;

use crate::types::ParseError;

use super::language::Language;
use super::locale_hint::LocaleHint;

crate::uuid_newtype! {
    /// Opaque identifier for a user row in `users`.
    pub UserId
}

crate::uuid_newtype! {
    /// Opaque identifier for an organization row.
    pub OrgId
}

crate::str_enum! {
    /// Role of a user within one org. Used by `org_members.role` and the
    /// JWT membership lookup.
    pub enum Role {
        Owner  => "owner",
        Admin  => "admin",
        Member => "member",
    }
}

/// RFC-ish email address.
///
/// We don't try to be a full RFC 5321 parser — just guard length and
/// the "must contain `@`" shape that downstream code relies on. The
/// Postgres `citext` column does the case-insensitive uniqueness work.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct Email(Arc<str>);

impl Email {
    pub const MAX_BYTES: usize = 320;
    pub const MIN_BYTES: usize = 3;

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for Email {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("Email").field(&self.as_str()).finish()
    }
}

impl std::fmt::Display for Email {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl TryFrom<&str> for Email {
    type Error = ParseError;

    fn try_from(raw: &str) -> Result<Self, Self::Error> {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Err(ParseError::Empty { field: "email" });
        }
        if trimmed.len() < Self::MIN_BYTES {
            return Err(ParseError::OutOfRange {
                field: "email",
                detail: "too short",
            });
        }
        if trimmed.len() > Self::MAX_BYTES {
            return Err(ParseError::TooLong {
                field: "email",
                max: Self::MAX_BYTES,
                got: trimmed.len(),
            });
        }
        // The `@` check is enough to refuse the obvious junk; deeper
        // validation belongs to the OAuth provider.
        let at = trimmed.find('@').ok_or(ParseError::Malformed {
            field: "email",
            detail: "missing @",
        })?;
        if at == 0 || at == trimmed.len() - 1 {
            return Err(ParseError::Malformed {
                field: "email",
                detail: "missing local or domain part",
            });
        }
        Ok(Self(Arc::from(trimmed)))
    }
}

impl TryFrom<String> for Email {
    type Error = ParseError;
    fn try_from(raw: String) -> Result<Self, Self::Error> {
        Self::try_from(raw.as_str())
    }
}

/// URL-safe organization slug. Mirrors the migration's CHECK regex:
/// `^[a-z0-9][a-z0-9-]{0,62}$`.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct OrgSlug(Arc<str>);

impl OrgSlug {
    pub const MAX_BYTES: usize = 63;

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for OrgSlug {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("OrgSlug").field(&self.as_str()).finish()
    }
}

impl std::fmt::Display for OrgSlug {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl TryFrom<&str> for OrgSlug {
    type Error = ParseError;

    fn try_from(raw: &str) -> Result<Self, Self::Error> {
        if raw.is_empty() {
            return Err(ParseError::Empty { field: "org_slug" });
        }
        if raw.len() > Self::MAX_BYTES {
            return Err(ParseError::TooLong {
                field: "org_slug",
                max: Self::MAX_BYTES,
                got: raw.len(),
            });
        }
        let mut chars = raw.chars();
        let first = chars
            .next()
            .ok_or(ParseError::Empty { field: "org_slug" })?;
        if !first.is_ascii_lowercase() && !first.is_ascii_digit() {
            return Err(ParseError::Malformed {
                field: "org_slug",
                detail: "must start with [a-z0-9]",
            });
        }
        for ch in chars {
            let ok = ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '-';
            if !ok {
                return Err(ParseError::Malformed {
                    field: "org_slug",
                    detail: "only [a-z0-9-] after the first char",
                });
            }
        }
        Ok(Self(Arc::from(raw)))
    }
}

impl TryFrom<String> for OrgSlug {
    type Error = ParseError;
    fn try_from(raw: String) -> Result<Self, Self::Error> {
        Self::try_from(raw.as_str())
    }
}

/// Google's `sub` claim — stable per-user identifier scoped to our
/// OAuth client. Persisted in `user_identities.subject` and used as
/// the primary key for "is this the same Google account."
///
/// Google publishes `sub` as a numeric string up to 255 chars; we
/// enforce non-empty + the 255-byte cap at the boundary.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct GoogleSubject(Arc<str>);

impl GoogleSubject {
    pub const MAX_BYTES: usize = 255;

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for GoogleSubject {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // PII — debug-tier per CLAUDE.md §2; redact in any printed form.
        f.write_str("GoogleSubject(***)")
    }
}

impl TryFrom<&str> for GoogleSubject {
    type Error = ParseError;
    fn try_from(raw: &str) -> Result<Self, Self::Error> {
        if raw.is_empty() {
            return Err(ParseError::Empty {
                field: "google_subject",
            });
        }
        if raw.len() > Self::MAX_BYTES {
            return Err(ParseError::TooLong {
                field: "google_subject",
                max: Self::MAX_BYTES,
                got: raw.len(),
            });
        }
        Ok(Self(Arc::from(raw)))
    }
}

impl TryFrom<String> for GoogleSubject {
    type Error = ParseError;
    fn try_from(raw: String) -> Result<Self, Self::Error> {
        Self::try_from(raw.as_str())
    }
}

/// RFC 7636 PKCE `code_verifier` — 43–128 chars from `[A-Za-z0-9\-._~]`.
/// Treated as secret material: redacted Debug, never logged.
#[derive(Clone, PartialEq, Eq)]
pub struct PkceVerifier(Arc<str>);

impl PkceVerifier {
    pub const MIN_BYTES: usize = 43;
    pub const MAX_BYTES: usize = 128;

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for PkceVerifier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PkceVerifier(***)")
    }
}

impl TryFrom<&str> for PkceVerifier {
    type Error = ParseError;
    fn try_from(raw: &str) -> Result<Self, Self::Error> {
        if raw.len() < Self::MIN_BYTES {
            return Err(ParseError::Malformed {
                field: "pkce_verifier",
                detail: "shorter than RFC 7636 minimum (43)",
            });
        }
        if raw.len() > Self::MAX_BYTES {
            return Err(ParseError::TooLong {
                field: "pkce_verifier",
                max: Self::MAX_BYTES,
                got: raw.len(),
            });
        }
        if !raw
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~'))
        {
            return Err(ParseError::Malformed {
                field: "pkce_verifier",
                detail: "non-unreserved character",
            });
        }
        Ok(Self(Arc::from(raw)))
    }
}

impl TryFrom<String> for PkceVerifier {
    type Error = ParseError;
    fn try_from(raw: String) -> Result<Self, Self::Error> {
        Self::try_from(raw.as_str())
    }
}

/// One-time CSRF nonce for the OAuth round-trip. Stored in
/// `oauth_login_states.state`. URL-safe; bounded so a hostile
/// callback cannot tip the row over a column-size cap.
#[derive(Clone, PartialEq, Eq)]
pub struct OAuthState(Arc<str>);

impl OAuthState {
    // Aligned with the `oauth_login_states.state` CHECK constraint
    // (octet_length BETWEEN 32 AND 128). Keeping the type stricter than
    // the bound a hostile caller might supply lets the DB serve as defense
    // in depth, not as the only validator.
    pub const MIN_BYTES: usize = 32;
    pub const MAX_BYTES: usize = 128;

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for OAuthState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Effectively a single-use secret; treat as redacted.
        f.write_str("OAuthState(***)")
    }
}

impl TryFrom<&str> for OAuthState {
    type Error = ParseError;
    fn try_from(raw: &str) -> Result<Self, Self::Error> {
        if raw.len() < Self::MIN_BYTES {
            return Err(ParseError::Malformed {
                field: "oauth_state",
                detail: "too short",
            });
        }
        if raw.len() > Self::MAX_BYTES {
            return Err(ParseError::TooLong {
                field: "oauth_state",
                max: Self::MAX_BYTES,
                got: raw.len(),
            });
        }
        if !raw
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~'))
        {
            return Err(ParseError::Malformed {
                field: "oauth_state",
                detail: "non-URL-safe character",
            });
        }
        Ok(Self(Arc::from(raw)))
    }
}

impl TryFrom<String> for OAuthState {
    type Error = ParseError;
    fn try_from(raw: String) -> Result<Self, Self::Error> {
        Self::try_from(raw.as_str())
    }
}

/// Profile claims pulled from Google's `userinfo` endpoint. Built by
/// [`crate::auth::oauth_google`] and consumed by [`UserStore::upsert_from_google`].
#[derive(Debug, Clone)]
pub struct GoogleProfile {
    pub subject: GoogleSubject,
    pub email: Email,
    pub email_verified: bool,
    pub display_name: Option<String>,
    pub avatar_url: Option<String>,
    /// BCP-47 locale tag (e.g. `"vi"`, `"en-US"`). Treated as a hint
    /// only — normalized into [`Language`] at the OAuth-callback boundary
    /// via [`Language::from_locale_hint`]. Bounded at the
    /// `fetch_userinfo` ingestion seam so the cross-module value carries
    /// a length invariant, not a free-form provider string.
    pub locale: Option<LocaleHint>,
}

/// Materialised user row.
#[derive(Debug, Clone)]
pub struct User {
    pub id: UserId,
    pub email: Email,
    pub display_name: Option<String>,
    pub avatar_url: Option<String>,
}

/// One row from `org_members` joined with `organizations`.
#[derive(Debug, Clone)]
pub struct OrgMembership {
    pub org_id: OrgId,
    pub org_name: String,
    pub org_slug: OrgSlug,
    pub role: Role,
    pub default_language: Language,
    /// `organizations.default_rule`. `None` when the org has not
    /// configured an `<organization-rule>` directive yet. Surfaced on
    /// `/me` so the FE editor seeds with the current value.
    pub default_rule: Option<super::org_rule::OrganizationRule>,
}

/// What every authed HTTP request hands to its handler. Built by the
/// [`crate::http::auth_layer`] middleware from the JWT cookie + a DB
/// membership lookup.
#[derive(Debug, Clone)]
pub struct Principal {
    pub user_id: UserId,
    pub active_org_id: OrgId,
    pub role: Role,
}
