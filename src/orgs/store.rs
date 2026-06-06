//! [`OrgStore`] trait and the row/page DTOs that flow through it.

use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};

use super::error::OrgError;
use crate::auth::{Email, InviteId, InviteToken, Language, OrgId, OrgName, OrgSlug, Role, UserId};
use crate::types::AvatarUrl;

pub type SharedOrgStore = Arc<dyn OrgStore>;

/// One row on the workspace-settings General tab.
#[derive(Debug, Clone)]
pub struct OrgDetails {
    pub id: OrgId,
    pub name: String,
    pub slug: OrgSlug,
    pub default_language: Language,
    pub created_at: DateTime<Utc>,
    pub member_count: i64,
    /// Public asset URL, or `None` → FE renders the default tile.
    pub avatar_url: Option<AvatarUrl>,
}

/// Bag of optional patches for `PATCH /me/org`. `None` means "don't
/// change this column" so the same handler can do partial updates
/// without diffing every field.
#[derive(Debug, Clone, Default)]
pub struct OrgUpdate {
    pub name: Option<OrgName>,
    pub slug: Option<OrgSlug>,
}

/// Status of a row on the Members tab. `Member` rows live in
/// `org_members`; `Invited` and `Expired` rows live in `org_invites`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemberStatus {
    Active,
    Invited,
    Expired,
}

impl MemberStatus {
    /// Wire / DB label. Used both in SQL bindings and the HTTP
    /// response shape so the FE filter tabs and `?status=` query
    /// parameter share one vocabulary.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Invited => "invited",
            Self::Expired => "expired",
        }
    }
}

/// One row on the Members tab — either a real `org_members` row or a
/// pending `org_invites` row. The two shapes share a column structure
/// so the UI can render them in one table.
#[derive(Debug, Clone)]
pub enum MemberRow {
    /// `org_members` row, joined with `users`.
    Member {
        user_id: UserId,
        email: Email,
        display_name: Option<String>,
        avatar_url: Option<AvatarUrl>,
        role: Role,
        joined_at: DateTime<Utc>,
    },
    /// Pending or expired invite.
    Invite(InviteRow),
}

/// Pending invite row exposed to the FE. The token cleartext never
/// flows back through this DTO — only at the moment of issuance via
/// [`IssuedInvite`].
#[derive(Debug, Clone)]
pub struct InviteRow {
    pub invite_id: InviteId,
    pub email: Email,
    pub role: Role,
    pub status: MemberStatus,
    pub invited_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
}

/// Filters passed to [`OrgStore::list_members`]. Combined into a
/// single struct so the trait method signature stays short and
/// future filters (e.g. seat status) can land without a SemVer churn.
#[derive(Debug, Clone)]
pub struct MemberFilter {
    pub query: Option<String>,
    pub status: Option<MemberStatus>,
    pub role: Option<Role>,
    pub page: u32,
    pub per_page: u32,
}

/// Page of [`MemberRow`] with cardinality counters for the filter
/// tabs in the design (`All N`, `Active N`, `Invited N`, `Expired N`).
#[derive(Debug, Clone)]
pub struct MemberPage {
    pub rows: Vec<MemberRow>,
    pub total: i64,
    pub active_count: i64,
    pub invited_count: i64,
    pub expired_count: i64,
}

/// Result of `POST /me/org/invites`: one row per submitted email.
/// `token` is the *cleartext* URL secret — only handed out at
/// issuance and never read back from the DB.
#[derive(Debug, Clone)]
pub struct IssuedInvite {
    pub invite_id: InviteId,
    pub email: Email,
    pub role: Role,
    pub token: InviteToken,
    pub expires_at: DateTime<Utc>,
}

/// Result of [`OrgStore::accept_invite`].
///
/// Carries the org the caller joined and the role they joined with. The
/// caller re-mints the session with `org_id` as the active org so the
/// invitee lands in the inviting workspace.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AcceptedInvite {
    pub org_id: OrgId,
    pub role: Role,
}

#[async_trait]
pub trait OrgStore: std::fmt::Debug + Send + Sync + 'static {
    /// Read the General-tab payload for the caller's active org.
    async fn read_org(&self, org_id: OrgId) -> Result<OrgDetails, OrgError>;

    /// Apply a partial patch. Returns the refreshed [`OrgDetails`] so
    /// the handler can echo it back without a re-read.
    async fn update_org(
        &self,
        org_id: OrgId,
        patch: OrgUpdate,
        now: DateTime<Utc>,
    ) -> Result<OrgDetails, OrgError>;

    /// Paginate the Members-tab rows for `org_id`.
    async fn list_members(
        &self,
        org_id: OrgId,
        filter: MemberFilter,
        now: DateTime<Utc>,
    ) -> Result<MemberPage, OrgError>;

    /// Change a member's role.
    ///
    /// Implementations must reject demoting or removing the last
    /// owner (see [`OrgError::LastOwnerProtected`]).
    async fn change_role(
        &self,
        org_id: OrgId,
        user_id: UserId,
        new_role: Role,
    ) -> Result<(), OrgError>;

    /// Remove a member from the org. Used by both "remove from
    /// workspace" (acting on another user) and "leave workspace"
    /// (acting on `principal.user_id`).
    async fn remove_member(&self, org_id: OrgId, user_id: UserId) -> Result<(), OrgError>;

    /// Create one invite per email. Idempotent on `(org_id, email)`
    /// for any *pending* row; an existing pending row has its token
    /// rotated and expiry extended.
    async fn create_invites(
        &self,
        org_id: OrgId,
        emails: &[Email],
        role: Role,
        invited_by: UserId,
        now: DateTime<Utc>,
        ttl: chrono::Duration,
    ) -> Result<Vec<IssuedInvite>, OrgError>;

    /// Re-issue a single pending invite. Rotates the token, refreshes
    /// the `expires_at`. Returns the new cleartext.
    async fn resend_invite(
        &self,
        org_id: OrgId,
        invite_id: InviteId,
        now: DateTime<Utc>,
        ttl: chrono::Duration,
    ) -> Result<IssuedInvite, OrgError>;

    /// Delete a pending invite (revoke).
    async fn revoke_invite(&self, org_id: OrgId, invite_id: InviteId) -> Result<(), OrgError>;

    /// Accept-on-login (ADR-0011, self-host invite-only). Consume every
    /// pending, non-expired invite addressed to `email` — inserting an
    /// `org_members` row with the invited role and stamping
    /// `consumed_at` — then return the org to make active (the oldest
    /// invite's org), or `None` when there is no pending invite.
    ///
    /// Runs privileged (the user is by definition not yet a member, so
    /// RLS on `org_invites` would hide the row). `email` must be the
    /// IdP-verified address from the id_token — possession of the
    /// invited mailbox is what authorises the join.
    async fn join_pending_invites(
        &self,
        user_id: UserId,
        email: &Email,
        now: DateTime<Utc>,
    ) -> Result<Option<OrgId>, OrgError>;

    /// Accept a single invite by its cleartext URL token (the link the
    /// invitee clicked). Unlike [`Self::join_pending_invites`], this is
    /// keyed on the token — possession of the secret is the
    /// authorisation — so it works for an already-authenticated user
    /// regardless of which email their account carries, and it switches
    /// them into the inviting org.
    ///
    /// Atomically claims the matching pending, non-expired invite
    /// (stamping `consumed_at`), inserts the `org_members` row with the
    /// invited role, and returns the joined org + role. Runs privileged
    /// (the user is by definition not yet a member, so RLS would hide
    /// the invite row).
    ///
    /// Errors distinguish the failure modes for the FE:
    /// [`OrgError::NotFound`] (no such token), [`OrgError::InviteExpired`]
    /// (past `expires_at`), [`OrgError::InviteAlreadyConsumed`] (already
    /// redeemed, including losing a concurrent-accept race).
    async fn accept_invite(
        &self,
        user_id: UserId,
        token: &InviteToken,
        now: DateTime<Utc>,
    ) -> Result<AcceptedInvite, OrgError>;

    /// Set (or clear, with `None`) the workspace avatar URL.
    async fn set_avatar_url(
        &self,
        org_id: OrgId,
        avatar_url: Option<&AvatarUrl>,
        now: DateTime<Utc>,
    ) -> Result<(), OrgError>;
}
