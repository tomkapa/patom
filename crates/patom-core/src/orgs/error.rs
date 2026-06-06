//! One error type for the `orgs` module boundary (CLAUDE.md §12).

use thiserror::Error;

use crate::auth::AuthError;
use crate::types::ParseError;

#[derive(Debug, Error)]
pub enum OrgError {
    /// Slug-uniqueness collision on `PATCH /me/org`. Maps to HTTP 409
    /// with body code `org_slug.taken`.
    #[error("org slug already in use")]
    SlugTaken,

    /// Caller tried to demote or remove the org's last owner. Maps to
    /// HTTP 409 with body code `org.last_owner`.
    #[error("at least one owner is required")]
    LastOwnerProtected,

    /// Target row missing — member id, invite id, etc. Maps to 404.
    #[error("not found")]
    NotFound,

    /// Pending invite has passed its `expires_at`. Maps to 410.
    #[error("invite expired")]
    InviteExpired,

    /// Pending invite has already been redeemed. Maps to 409.
    #[error("invite already consumed")]
    InviteAlreadyConsumed,

    /// More than [`super::MAX_INVITE_BATCH`] emails were submitted in
    /// one call. The caller should split the batch.
    #[error("invite batch too large: max {max}, got {got}")]
    InviteBatchTooLarge { max: usize, got: usize },

    /// Smart-constructor failure parsing an inbound body.
    #[error("parse: {0}")]
    Parse(#[from] ParseError),

    /// Auth-side failure surfaced through the privileged tx layer
    /// (membership lookup, etc.).
    #[error("auth: {0}")]
    Auth(#[from] AuthError),

    /// Generic Postgres failure that isn't an RLS denial.
    #[error("db: {0}")]
    Db(#[from] sqlx::Error),
}
