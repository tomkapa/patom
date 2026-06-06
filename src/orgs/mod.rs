//! Workspace settings + members + invites.
//!
//! Surfaces the data behind the General and Members tabs of the
//! workspace-settings page. The auth crate owns identity row creation
//! (OAuth callback path) and the `/me` projection; this crate owns
//! everything an authenticated admin would *do* with a workspace
//! they already belong to.
//!
//! The split is deliberate: keeping admin mutations out of the
//! `auth::UserStore` trait stops that trait from sprouting one method
//! per UI screen.

mod error;
mod limits;
pub mod mailer;
mod pg_store;
mod store;

pub use error::OrgError;
pub use limits::{INVITE_TTL, MAX_INVITE_BATCH, MAX_MEMBERS_PER_PAGE};
pub use mailer::{LogMailer, MailError, Mailer, SharedMailer, SmtpMailer};
pub use pg_store::PgOrgStore;
pub use store::{
    AcceptedInvite, InviteRow, IssuedInvite, MemberFilter, MemberPage, MemberRow, MemberStatus,
    OrgDetails, OrgStore, OrgUpdate, SharedOrgStore,
};
