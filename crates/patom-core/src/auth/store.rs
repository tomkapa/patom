//! `UserStore` — the only auth-side trait. Defines insert/lookup over
//! `users`, `user_identities`, `organizations`, `org_members`, and the
//! short-lived `oauth_login_states` rows.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};

use crate::types::AvatarUrl;

use super::error::AuthError;
use super::language::Language;
use super::locale_hint::LocaleHint;
use super::org_rule::OrganizationRule;
use super::types::{
    Email, OAuthState, OidcNonce, OidcProfile, OrgId, OrgMembership, PkceVerifier, Role, User,
    UserId,
};

pub type SharedUserStore = Arc<dyn UserStore>;

/// New row to be inserted into `oauth_login_states`.
#[derive(Debug, Clone)]
pub struct OAuthStateRow {
    pub state: OAuthState,
    pub pkce_verifier: PkceVerifier,
    /// id_token `nonce` the authorize step minted; replayed in the
    /// callback to bind the id_token to this exact login round-trip.
    pub nonce: OidcNonce,
    pub redirect_to: Option<String>,
    /// Inbound `Accept-Language` primary tag captured at
    /// `/auth/oidc/login`. Replayed in the callback as a fallback when
    /// the id_token `locale` claim is missing. Bounded by [`LocaleHint`];
    /// the column CHECK is the defence-in-depth backstop.
    pub detected_locale: Option<LocaleHint>,
    pub created_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
}

/// Row returned when consuming a `oauth_login_states` row.
#[derive(Debug, Clone)]
pub struct ConsumedOAuthState {
    pub pkce_verifier: PkceVerifier,
    /// The id_token `nonce` stashed at login; the callback hands it to
    /// [`super::OidcAuth::exchange`] to verify the id_token.
    pub nonce: OidcNonce,
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

/// Minimal display identity for a user — the name and avatar a viewer
/// needs to render someone *else's* message bubble.
///
/// Exists because tenant-scoped routes run as `patom_app` and cannot
/// JOIN `users` (migration 14), yet the chat surfaces must show the real
/// author of every human row, not the current viewer. `name` is already
/// resolved server-side with the roster formula
/// `COALESCE(display_name, split_part(email, '@', 1))` (see
/// `colleagues/pg_store.rs`), so the caller renders it verbatim. The
/// avatar is the raw DB string (already length-checked by the column
/// CHECK) — a display read never fails on a malformed value.
#[derive(Debug, Clone)]
pub struct UserProfileLite {
    pub name: String,
    pub avatar_url: Option<String>,
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
    /// Insert or update the user + identity rows that map to one verified
    /// OIDC profile. Idempotent on `(oidc_issuer, oidc_subject)`.
    async fn upsert_from_oidc(
        &self,
        profile: &OidcProfile,
        now: DateTime<Utc>,
    ) -> Result<UpsertedUser, AuthError>;

    /// Create a personal organisation for a user. Returns the new org;
    /// also inserts an `org_members` row with role = Owner. The
    /// `default_language` is the per-org language picked by the OAuth
    /// callback from the user's locale hints — see
    /// [`Language::from_locale_hint`].
    ///
    /// `cap` enforces a per-user owned-org ceiling **atomically**: when
    /// `Some(n)`, the implementation takes a per-user advisory lock and
    /// re-counts the caller's Owner memberships inside the same
    /// transaction as the insert, returning
    /// [`AuthError::OrgLimitReached`] when already at `n`. This closes the
    /// check-then-insert race that a separate count + insert would leave
    /// open. `None` skips the check — the OAuth callback's auto-created
    /// personal org is never capped.
    async fn create_personal_org(
        &self,
        user_id: UserId,
        suggested_slug: &str,
        display_name: &str,
        language: Language,
        cap: Option<i64>,
        now: DateTime<Utc>,
    ) -> Result<NewOrg, AuthError>;

    /// First-admin bootstrap (ADR-0011 §3). When the `organizations`
    /// table is empty, create the initial org and make `user_id` its
    /// owner — returning `Some(org)`. When the table is non-empty,
    /// perform no write and return `None` so the caller falls through to
    /// the normal self-service path.
    ///
    /// The emptiness check and the insert run in **one** privileged
    /// transaction guarded by an advisory lock, so two simultaneous
    /// first logins cannot both bootstrap (§6). Only ever called when the
    /// operator set `PATOM_BOOTSTRAP_ADMIN`.
    async fn bootstrap_initial_org_as_owner(
        &self,
        user_id: UserId,
        suggested_slug: &str,
        display_name: &str,
        language: Language,
        now: DateTime<Utc>,
    ) -> Result<Option<NewOrg>, AuthError>;

    /// List every org the user belongs to.
    async fn list_user_orgs(&self, user_id: UserId) -> Result<Vec<OrgMembership>, AuthError>;

    /// Return the user's role in `org_id`, or `None` if they're not a
    /// member.
    async fn membership(&self, user_id: UserId, org_id: OrgId) -> Result<Option<Role>, AuthError>;

    /// Look up a user by id (for `/me`).
    async fn read_user(&self, user_id: UserId) -> Result<Option<User>, AuthError>;

    /// Batched email lookup keyed by user id.
    ///
    /// Exists so tenant-scoped routes (which run as `patom_app` and
    /// therefore cannot JOIN onto `users` — see migration 14) can still
    /// surface creator emails without going through the privileged
    /// store on every row. Missing ids are omitted from the map; the
    /// caller decides how to render an absent value. Duplicates in
    /// `ids` are deduped on the SQL side via `WHERE id = ANY($1)`.
    async fn read_emails(&self, ids: &[UserId]) -> Result<HashMap<UserId, Email>, AuthError>;

    /// Batched display-identity lookup keyed by user id.
    ///
    /// Same rationale as [`Self::read_emails`]: tenant-scoped chat routes
    /// run as `patom_app` and cannot JOIN `users` (migration 14), but the
    /// feed and thread views must render the *real* author's name and
    /// avatar. The name is resolved with the roster
    /// `COALESCE(display_name, split_part(email, '@', 1))` formula so it
    /// matches the `<colleagues>` roster exactly. Missing ids are omitted;
    /// duplicates in `ids` are deduped via `WHERE id = ANY($1)`.
    async fn read_profiles(
        &self,
        ids: &[UserId],
    ) -> Result<HashMap<UserId, UserProfileLite>, AuthError>;

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
        avatar_url: Option<&AvatarUrl>,
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
