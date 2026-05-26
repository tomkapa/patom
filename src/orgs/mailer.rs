//! Outbound transactional mail.
//!
//! Production SMTP / SES wiring is not in tree yet. To keep the
//! invite flow shippable, [`LogMailer`] writes a structured
//! `tracing::info!` event in place of an SMTP send so an operator
//! can read the link out of the logs. Swapping in a real impl is a
//! single `Arc::new(...)` change in `main.rs`.

use std::sync::Arc;

use async_trait::async_trait;

use crate::auth::{Email, InviteToken, OrgSlug, Role};

pub type SharedMailer = Arc<dyn Mailer>;

/// One outbound invite. Held by value because the caller (the HTTP
/// handler) has already validated each component through the
/// boundary newtypes.
#[derive(Debug, Clone)]
pub struct InviteMail<'a> {
    pub to: &'a Email,
    pub org_name: &'a str,
    pub org_slug: &'a OrgSlug,
    pub role: Role,
    pub token: &'a InviteToken,
    pub web_base_url: Option<&'a str>,
}

#[async_trait]
pub trait Mailer: std::fmt::Debug + Send + Sync + 'static {
    /// Best-effort deliver. The trait surface is fire-and-forget on
    /// purpose: a transient SMTP outage must not roll back the
    /// `org_invites` row, because the operator can resend later.
    async fn send_invite(&self, mail: InviteMail<'_>);
}

/// Tracing-only mailer. The "send" is one structured event so the
/// link is recoverable from logs in dev / staging.
#[derive(Debug, Default)]
pub struct LogMailer;

#[async_trait]
impl Mailer for LogMailer {
    async fn send_invite(&self, mail: InviteMail<'_>) {
        // The token is sensitive — emitted only at DEBUG so prod
        // logs do not leak invite material. The accept URL is built
        // at INFO so operators can confirm the *shape* of the
        // outbound mail without exposing the secret.
        let base = mail.web_base_url.unwrap_or("https://relay.app");
        tracing::info!(
            event = "org.invite.sent",
            relay.org.slug = %mail.org_slug,
            relay.invite.role = %mail.role.as_str(),
            mail.to = %mail.to,
            mail.accept_url_host = %base,
        );
        tracing::debug!(
            event = "org.invite.sent.token",
            mail.to = %mail.to,
            relay.invite.token = mail.token.as_str(),
        );
    }
}
