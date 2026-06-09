//! Per CLAUDE.md §12: one error type for the `auth` module boundary.

use thiserror::Error;

use crate::types::ParseError;

use super::types::OrgId;

#[derive(Debug, Error)]
pub enum AuthError {
    /// No cookie, expired JWT, or signature mismatch. Cookie absence and
    /// JWT failures collapse to one variant so the surface that touches
    /// the network can't accidentally leak whether the token *was*
    /// present.
    #[error("missing or invalid session")]
    Unauthenticated,

    /// JWT verified but the user is not a member of the org named in
    /// the `org` claim. Distinct from `Unauthenticated` because the
    /// caller is identified — we just refuse the scope.
    #[error("user not a member of org {0}")]
    NotMember(OrgId),

    /// OAuth callback arrived without (or after) a valid `state` row.
    /// Either CSRF, replay, or the user took longer than the TTL.
    #[error("oauth state expired or unknown")]
    OAuthStateInvalid,

    /// The remote OIDC provider returned an error envelope, an
    /// unparseable response, or an id_token that failed signature /
    /// nonce verification.
    #[error("oauth provider error: {0}")]
    OAuthProvider(String),

    /// OIDC discovery (`{issuer}/.well-known/openid-configuration` + its
    /// JWKS) failed or timed out at startup. Fail-closed: a provider we
    /// can't discover yields no login rather than a degraded path.
    #[error("oidc discovery failed: {0}")]
    DiscoveryFailed(String),

    /// The provider claims the email is unverified. We refuse to mint a
    /// session for it: an unverified email must not seed a `users.email`
    /// row that other surfaces treat as a confirmed contact.
    #[error("email not verified by oauth provider")]
    EmailUnverified,

    /// JWT crate raised an error we don't categorise further (signing
    /// init failure, malformed claims).
    #[error("jwt: {0}")]
    Jwt(String),

    /// Smart-constructor failure when parsing OAuth profile fields.
    #[error("parse: {0}")]
    Parse(#[from] ParseError),

    /// Self-service workspace creation refused: the caller already owns
    /// the maximum number of workspaces (see
    /// [`super::limits::MAX_ORGS_PER_USER`]). Maps to 409.
    #[error("workspace limit reached: max {max} owned per user")]
    OrgLimitReached { max: i64 },

    /// Postgres-side failure that isn't an RLS denial.
    #[error("db: {0}")]
    Db(#[from] sqlx::Error),

    /// Internal misconfiguration (missing secret at boundary code path
    /// that should have validated at startup, etc.). Triggers 500.
    #[error("internal: {0}")]
    Internal(&'static str),

    /// Same intent as [`AuthError::Internal`] but carrying a dynamic
    /// message — e.g. a failed URL parse during OAuth client
    /// construction where the underlying error string only exists at
    /// runtime. Triggers 500.
    #[error("misconfigured: {0}")]
    Misconfigured(String),
}
