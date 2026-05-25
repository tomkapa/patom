//! `UserStore` — the only auth-side trait. Defines insert/lookup over
//! `users`, `user_identities`, `organizations`, `org_members`, and the
//! short-lived `oauth_login_states` rows.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};

use super::error::AuthError;
use super::language::Language;
use super::locale_hint::LocaleHint;
use super::org_rule::OrganizationRule;
use super::types::{
    Email, GoogleProfile, OAuthState, OrgId, OrgMembership, PkceVerifier, Role, User, UserId,
};

pub type SharedUserStore = Arc<dyn UserStore>;

/// New row to be inserted into `oauth_login_states`.
#[derive(Debug, Clone)]
pub struct OAuthStateRow {
    pub state: OAuthState,
    pub pkce_verifier: PkceVerifier,
    pub redirect_to: Option<String>,
    /// Inbound `Accept-Language` primary tag captured at
    /// `/auth/google/login`. Replayed in the callback as a fallback when
    /// Google's userinfo `locale` is missing. Bounded by [`LocaleHint`];
    /// the column CHECK is the defence-in-depth backstop.
    pub detected_locale: Option<LocaleHint>,
    pub created_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
}

/// Row returned when consuming a `oauth_login_states` row.
#[derive(Debug, Clone)]
pub struct ConsumedOAuthState {
    pub pkce_verifier: PkceVerifier,
    pub redirect_to: Option<String>,
    /// The same `Accept-Language` primary tag the login round-trip
    /// stashed; consumed by the callback to derive the personal org's
    /// `default_language` via [`Language::from_locale_hint`].
    pub detected_locale: Option<LocaleHint>,
}

/// Result of an OAuth upsert. `is_new_user` lets the caller branch on
/// "first sign-up, seed personal org" without an extra round trip.
#[derive(Debug, Clone)]
pub struct UpsertedUser {
    pub user: User,
    pub is_new_user: bool,
}

/// A freshly-created organisation row.
#[derive(Debug, Clone)]
pub struct NewOrg {
    pub id: OrgId,
    pub slug: String,
    pub name: String,
    pub default_language: Language,
}

#[async_trait]
pub trait UserStore: std::fmt::Debug + Send + Sync + 'static {
    /// Insert or update the user + identity rows that map to one Google
    /// profile. Idempotent on `(provider, subject)`.
    async fn upsert_from_google(
        &self,
        profile: &GoogleProfile,
        now: DateTime<Utc>,
    ) -> Result<UpsertedUser, AuthError>;

    /// Create a personal organisation for a user. Returns the new org;
    /// also inserts an `org_members` row with role = Owner. The
    /// `default_language` is the per-org language picked by the OAuth
    /// callback from the user's locale hints — see
    /// [`Language::from_locale_hint`].
    async fn create_personal_org(
        &self,
        user_id: UserId,
        suggested_slug: &str,
        display_name: &str,
        language: Language,
        now: DateTime<Utc>,
    ) -> Result<NewOrg, AuthError>;

    /// List every org the user belongs to.
    async fn list_user_orgs(&self, user_id: UserId) -> Result<Vec<OrgMembership>, AuthError>;

    /// Return the user's role in `org_id`, or `None` if they're not a
    /// member.
    async fn membership(&self, user_id: UserId, org_id: OrgId) -> Result<Option<Role>, AuthError>;

    /// Look up a user by id (for `/me`).
    async fn read_user(&self, user_id: UserId) -> Result<Option<User>, AuthError>;

    /// Batched email lookup keyed by user id.
    ///
    /// Exists so tenant-scoped routes (which run as `relay_app` and
    /// therefore cannot JOIN onto `users` — see migration 14) can still
    /// surface creator emails without going through the privileged
    /// store on every row. Missing ids are omitted from the map; the
    /// caller decides how to render an absent value. Duplicates in
    /// `ids` are deduped on the SQL side via `WHERE id = ANY($1)`.
    async fn read_emails(&self, ids: &[UserId]) -> Result<HashMap<UserId, Email>, AuthError>;

    /// Read the org's `default_language`. Called by the language
    /// resolver on cache miss; the column is NOT NULL, so a missing row
    /// surfaces as an `AuthError`.
    async fn read_org_language(&self, org_id: OrgId) -> Result<Language, AuthError>;

    /// Set the org's `default_language`. Returns the persisted value
    /// (always equal to `language` on success) so the handler can echo
    /// it back without a re-read.
    async fn set_org_language(
        &self,
        org_id: OrgId,
        language: Language,
        now: DateTime<Utc>,
    ) -> Result<Language, AuthError>;

    /// Read the org's `default_rule`. Called by the rule resolver on
    /// cache miss. The column is nullable — `Ok(None)` is the "no rule
    /// configured" state and is plumbed all the way through the
    /// renderer (which omits the `<organization-rule>` tag entirely).
    /// A missing row is itself a wiring bug; the impl surfaces it as
    /// [`AuthError::Internal`] to match `read_org_language`.
    async fn read_org_rule(&self, org_id: OrgId) -> Result<Option<OrganizationRule>, AuthError>;

    /// Set the org's `default_rule`. `None` clears the rule (sets the
    /// column to NULL). Returns the persisted value so the handler can
    /// echo it back without a re-read.
    async fn set_org_rule(
        &self,
        org_id: OrgId,
        rule: Option<OrganizationRule>,
        now: DateTime<Utc>,
    ) -> Result<Option<OrganizationRule>, AuthError>;

    /// Set the user's avatar URL. `None` clears it. Returns
    /// [`AuthError::Unauthenticated`] mapped from a missing row — the
    /// handler treats that as "user vanished mid-request", same as
    /// `read_user` returning `None`.
    async fn set_avatar_url(
        &self,
        user_id: UserId,
        avatar_url: Option<&str>,
        now: DateTime<Utc>,
    ) -> Result<(), AuthError>;

    /// Insert a `oauth_login_states` row. Caller has minted the random
    /// `state` + PKCE verifier.
    async fn insert_oauth_state(&self, row: &OAuthStateRow) -> Result<(), AuthError>;

    /// Atomically consume an `oauth_login_states` row by `state`. Deletes
    /// the row on success and returns the stored verifier; returns
    /// [`AuthError::OAuthStateInvalid`] when the row is missing or
    /// expired.
    async fn consume_oauth_state(
        &self,
        state: &OAuthState,
        now: DateTime<Utc>,
    ) -> Result<ConsumedOAuthState, AuthError>;
}
