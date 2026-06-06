//! Identity-surface newtypes. Per CLAUDE.md §1 every value carrying an
//! invariant is a newtype with a `TryFrom` smart constructor.

use std::sync::Arc;

use crate::types::{AvatarUrl, ParseError};

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

/// OIDC `sub` claim — the stable, per-issuer identifier for an end
/// user. Persisted in `user_identities.oidc_subject` and, together with
/// the issuer, forms the primary key for "is this the same account."
///
/// `sub` is opaque and provider-defined (Google emits a numeric string,
/// Entra a GUID, Keycloak a UUID); we only enforce non-empty + the
/// 255-byte cap at the boundary, matching the column CHECK.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct OidcSubject(Arc<str>);

impl OidcSubject {
    pub const MAX_BYTES: usize = 255;

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for OidcSubject {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // PII — debug-tier per CLAUDE.md §2; redact in any printed form.
        f.write_str("OidcSubject(***)")
    }
}

impl TryFrom<&str> for OidcSubject {
    type Error = ParseError;
    fn try_from(raw: &str) -> Result<Self, Self::Error> {
        if raw.is_empty() {
            return Err(ParseError::Empty {
                field: "oidc_subject",
            });
        }
        if raw.len() > Self::MAX_BYTES {
            return Err(ParseError::TooLong {
                field: "oidc_subject",
                max: Self::MAX_BYTES,
                got: raw.len(),
            });
        }
        Ok(Self(Arc::from(raw)))
    }
}

impl TryFrom<String> for OidcSubject {
    type Error = ParseError;
    fn try_from(raw: String) -> Result<Self, Self::Error> {
        Self::try_from(raw.as_str())
    }
}

/// The OIDC issuer identifier — the `https` origin that names an IdP.
///
/// E.g. `https://accounts.google.com`. Half of the `(issuer, subject)`
/// identity key and the seed for discovery
/// (`{issuer}/.well-known/openid-configuration`).
///
/// Parsed at the config boundary: must be a syntactically valid URL with
/// an `https` scheme (OIDC forbids a plaintext issuer) and no query or
/// fragment. The 255-byte cap matches the `user_identities.oidc_issuer`
/// column CHECK.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct IssuerUrl(Arc<str>);

impl IssuerUrl {
    pub const MAX_BYTES: usize = 255;

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for IssuerUrl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("IssuerUrl").field(&self.as_str()).finish()
    }
}

impl std::fmt::Display for IssuerUrl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl TryFrom<&str> for IssuerUrl {
    type Error = ParseError;

    fn try_from(raw: &str) -> Result<Self, Self::Error> {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Err(ParseError::Empty {
                field: "oidc_issuer",
            });
        }
        if trimmed.len() > Self::MAX_BYTES {
            return Err(ParseError::TooLong {
                field: "oidc_issuer",
                max: Self::MAX_BYTES,
                got: trimmed.len(),
            });
        }
        let parsed = url::Url::parse(trimmed).map_err(|_| ParseError::Malformed {
            field: "oidc_issuer",
            detail: "not a valid URL",
        })?;
        if parsed.scheme() != "https" {
            return Err(ParseError::Malformed {
                field: "oidc_issuer",
                detail: "scheme must be https",
            });
        }
        if parsed.query().is_some() || parsed.fragment().is_some() {
            return Err(ParseError::Malformed {
                field: "oidc_issuer",
                detail: "must not carry a query or fragment",
            });
        }
        Ok(Self(Arc::from(trimmed)))
    }
}

impl TryFrom<String> for IssuerUrl {
    type Error = ParseError;
    fn try_from(raw: String) -> Result<Self, Self::Error> {
        Self::try_from(raw.as_str())
    }
}

/// OIDC `nonce` — a single-use value binding the id_token to one login.
///
/// The authorize step mints it and the callback replays it so the
/// id_token can be bound to this exact round-trip (defeats id_token
/// replay). Stored in `oauth_login_states.nonce`; secret-tier, so
/// redacted Debug.
#[derive(Clone, PartialEq, Eq)]
pub struct OidcNonce(Arc<str>);

impl OidcNonce {
    // Matches the `oauth_login_states.nonce` CHECK (octet_length 1..=128).
    pub const MIN_BYTES: usize = 1;
    pub const MAX_BYTES: usize = 128;

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for OidcNonce {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("OidcNonce(***)")
    }
}

impl TryFrom<&str> for OidcNonce {
    type Error = ParseError;
    fn try_from(raw: &str) -> Result<Self, Self::Error> {
        if raw.len() < Self::MIN_BYTES {
            return Err(ParseError::Empty {
                field: "oidc_nonce",
            });
        }
        if raw.len() > Self::MAX_BYTES {
            return Err(ParseError::TooLong {
                field: "oidc_nonce",
                max: Self::MAX_BYTES,
                got: raw.len(),
            });
        }
        Ok(Self(Arc::from(raw)))
    }
}

impl TryFrom<String> for OidcNonce {
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

/// Verified identity claims from an OIDC id_token.
///
/// Built by [`crate::auth::oidc`] after JWKS signature + nonce
/// verification and consumed by [`UserStore::upsert_from_oidc`]. Keyed on
/// `(issuer, subject)`; email is captured for display, not identity.
#[derive(Debug, Clone)]
pub struct OidcProfile {
    /// The issuer the id_token was verified against — the trusted half
    /// of the `(issuer, subject)` identity key.
    pub issuer: IssuerUrl,
    pub subject: OidcSubject,
    pub email: Email,
    pub email_verified: bool,
    pub display_name: Option<String>,
    /// IdP `picture` claim, parsed at the OIDC boundary. A claim that
    /// isn't a valid avatar URL is dropped to `None` (best-effort hint,
    /// like `locale`) rather than failing sign-in.
    pub avatar_url: Option<AvatarUrl>,
    /// BCP-47 locale tag (e.g. `"vi"`, `"en-US"`). Treated as a hint
    /// only — normalized into [`Language`] at the OAuth-callback boundary
    /// via [`Language::from_locale_hint`]. Bounded at the id_token
    /// ingestion seam so the cross-module value carries a length
    /// invariant, not a free-form provider string.
    pub locale: Option<LocaleHint>,
}

/// Materialised user row.
#[derive(Debug, Clone)]
pub struct User {
    pub id: UserId,
    pub email: Email,
    pub display_name: Option<String>,
    pub avatar_url: Option<AvatarUrl>,
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
    /// `organizations.avatar_url`. `None` → FE renders the default tile.
    pub avatar_url: Option<AvatarUrl>,
    /// `organizations.onboarded_at IS NOT NULL`. `false` means the user
    /// hasn't walked the /onboarding wizard yet — the FE gate routes
    /// them there until the wizard's final step flips this via PATCH
    /// /me/org { onboarded: true }. Existing rows backfill to true in
    /// migration 57 so live users aren't shoved into the wizard.
    pub onboarded: bool,
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

/// Editable display name for an organization.
///
/// The schema's `organizations.name` column is `octet_length BETWEEN
/// 1 AND 200`. Whitespace-only inputs are rejected at this seam so the
/// FE can't slip an empty-looking name past the CHECK with a row of
/// spaces.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct OrgName(Arc<str>);

impl OrgName {
    pub const MAX_BYTES: usize = super::limits::MAX_ORG_NAME_BYTES;

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for OrgName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("OrgName").field(&self.as_str()).finish()
    }
}

impl std::fmt::Display for OrgName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl TryFrom<&str> for OrgName {
    type Error = ParseError;

    fn try_from(raw: &str) -> Result<Self, Self::Error> {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Err(ParseError::Empty { field: "org_name" });
        }
        if trimmed.len() > Self::MAX_BYTES {
            return Err(ParseError::TooLong {
                field: "org_name",
                max: Self::MAX_BYTES,
                got: trimmed.len(),
            });
        }
        Ok(Self(Arc::from(trimmed)))
    }
}

impl TryFrom<String> for OrgName {
    type Error = ParseError;
    fn try_from(raw: String) -> Result<Self, Self::Error> {
        Self::try_from(raw.as_str())
    }
}

crate::uuid_newtype! {
    /// Opaque identifier for one `org_invites` row.
    pub InviteId
}

/// Single-use invite secret.
///
/// URL-safe base64 of 32 random bytes (≈43 chars). Stored hashed at
/// rest (`org_invites.token_hash` = SHA-256 of the cleartext); the
/// cleartext only ever exists in memory at the moment of issuance and
/// is handed to the FE once in the invite-mail link.
#[derive(Clone, PartialEq, Eq)]
pub struct InviteToken(Arc<str>);

impl InviteToken {
    /// 32 bytes of entropy → 43 chars of unpadded base64url.
    pub const RAW_ENTROPY_BYTES: usize = 32;
    pub const MIN_BYTES: usize = 43;
    pub const MAX_BYTES: usize = 64;

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for InviteToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Treat as a secret in any printed form.
        f.write_str("InviteToken(***)")
    }
}

impl TryFrom<&str> for InviteToken {
    type Error = ParseError;

    fn try_from(raw: &str) -> Result<Self, Self::Error> {
        if raw.len() < Self::MIN_BYTES {
            return Err(ParseError::Malformed {
                field: "invite_token",
                detail: "too short",
            });
        }
        if raw.len() > Self::MAX_BYTES {
            return Err(ParseError::TooLong {
                field: "invite_token",
                max: Self::MAX_BYTES,
                got: raw.len(),
            });
        }
        if !raw
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
        {
            return Err(ParseError::Malformed {
                field: "invite_token",
                detail: "non-URL-safe character",
            });
        }
        Ok(Self(Arc::from(raw)))
    }
}

impl TryFrom<String> for InviteToken {
    type Error = ParseError;
    fn try_from(raw: String) -> Result<Self, Self::Error> {
        Self::try_from(raw.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn issuer_url_accepts_https_origin() {
        let iss = IssuerUrl::try_from("https://accounts.google.com").expect("valid https issuer");
        assert_eq!(iss.as_str(), "https://accounts.google.com");
    }

    #[test]
    fn issuer_url_rejects_plaintext_scheme() {
        // OIDC forbids a plaintext issuer; the boundary must refuse http://.
        assert!(IssuerUrl::try_from("http://accounts.google.com").is_err());
    }

    #[test]
    fn issuer_url_rejects_query_and_fragment() {
        assert!(IssuerUrl::try_from("https://idp.test/?a=b").is_err());
        assert!(IssuerUrl::try_from("https://idp.test/#frag").is_err());
    }

    #[test]
    fn issuer_url_rejects_garbage_and_oversize() {
        assert!(IssuerUrl::try_from("not a url").is_err());
        let long = format!("https://{}.test", "a".repeat(IssuerUrl::MAX_BYTES));
        assert!(IssuerUrl::try_from(long.as_str()).is_err());
    }

    #[test]
    fn oidc_subject_enforces_bounds() {
        assert!(OidcSubject::try_from("").is_err());
        assert!(OidcSubject::try_from("sub-123").is_ok());
        let max = "a".repeat(OidcSubject::MAX_BYTES);
        assert!(OidcSubject::try_from(max.as_str()).is_ok());
        let over = "a".repeat(OidcSubject::MAX_BYTES + 1);
        assert!(OidcSubject::try_from(over.as_str()).is_err());
    }

    #[test]
    fn oidc_subject_debug_is_redacted() {
        let sub = OidcSubject::try_from("sensitive-sub").expect("valid");
        assert_eq!(format!("{sub:?}"), "OidcSubject(***)");
    }

    #[test]
    fn oidc_nonce_enforces_bounds_and_redacts() {
        assert!(OidcNonce::try_from("").is_err());
        assert!(OidcNonce::try_from("n").is_ok());
        let over = "a".repeat(OidcNonce::MAX_BYTES + 1);
        assert!(OidcNonce::try_from(over.as_str()).is_err());
        let nonce = OidcNonce::try_from("abc123").expect("valid");
        assert_eq!(format!("{nonce:?}"), "OidcNonce(***)");
    }
}
