//! Outbound transactional mail.
//!
//! Two implementations sit behind the [`Mailer`] trait. [`SmtpMailer`]
//! delivers over a real SMTP relay and is wired when the operator sets the
//! `PATOM_SMTP_*` env vars (see [`crate::config::SmtpSettings`]).
//! [`LogMailer`] is the fallback: it writes a structured `tracing` event in
//! place of a send so the invite link is recoverable from the logs. The
//! choice is a single `match` in `app.rs`; tests always use [`LogMailer`].

use std::sync::Arc;

use async_trait::async_trait;
use lettre::message::{Mailbox, MultiPart};
use lettre::transport::smtp::authentication::Credentials;
use lettre::{Address, AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor};
use thiserror::Error;

use crate::auth::{Email, InviteToken, OrgSlug, Role};
use crate::config::SmtpSettings;
use crate::orgs::limits::EMAIL_SEND_TIMEOUT;

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
        // Recipient email and token cleartext are both PII / secret —
        // emit only at DEBUG per CLAUDE.md §2 so production exporters
        // can strip them. INFO carries only the non-identifying shape
        // (which org, what role, which host the link points at). All
        // custom attributes sit under the `patom.*` namespace.
        let base = mail.web_base_url.unwrap_or("https://patom.app");
        tracing::info!(
            event = "org.invite.sent",
            patom.org.slug = %mail.org_slug,
            patom.invite.role = %mail.role.as_str(),
            patom.invite.accept_url_host = %base,
        );
        tracing::debug!(
            event = "org.invite.sent.token",
            patom.invite.recipient = %mail.to,
            patom.invite.token = mail.token.as_str(),
        );
    }
}

/// Fallback host when no `web_base_url` is configured. Matches the apex the
/// FE invite-share affordance points at, and [`LogMailer`]'s own fallback.
const FALLBACK_WEB_BASE: &str = "https://patom.app";

/// Failures building or dispatching a message.
///
/// The [`Mailer`] trait is fire-and-forget, so `send_invite` consumes these
/// internally (logging at `ERROR`); they surface as a `Result` only from
/// [`SmtpMailer::try_new`], where a bad relay config must fail fast at
/// startup (CLAUDE.md §12).
#[derive(Debug, Error)]
pub enum MailError {
    /// An address (the `From` mailbox or a recipient) is not a valid
    /// RFC 5321 mailbox once handed to the SMTP layer.
    #[error("invalid email address: {raw:?}")]
    Address { raw: String },
    /// The MIME message could not be assembled (e.g. a header rejected a
    /// control character).
    #[error("building message: {0}")]
    Build(#[from] lettre::error::Error),
    /// The SMTP transport could not be constructed (TLS/relay setup).
    #[error("building smtp transport: {0}")]
    Transport(#[from] lettre::transport::smtp::Error),
}

/// The three rendered representations of one invite email. Pure output of
/// [`render_invite`] so the copy and link logic is unit-tested without a
/// transport (CLAUDE.md §3).
#[derive(Debug, Clone, PartialEq, Eq)]
struct RenderedInvite {
    subject: String,
    text: String,
    html: String,
}

/// Build the accept link the recipient clicks. Mirrors the FE share-link
/// shape `{base}/i/{slug}/{token}` so a mailed link and a copied link are
/// identical. `web_base_url` is a validated origin (no trailing slash); the
/// trim is belt-and-suspenders against the fallback drifting.
fn accept_url(mail: &InviteMail<'_>) -> String {
    let base = mail
        .web_base_url
        .unwrap_or(FALLBACK_WEB_BASE)
        .trim_end_matches('/');
    format!("{base}/i/{}/{}", mail.org_slug, mail.token.as_str())
}

/// Minimal HTML-entity escape for the one piece of operator-controlled text
/// that lands in the HTML body — the workspace display name. The accept URL
/// (URL-safe token, regex-constrained slug) and role (closed enum) carry no
/// HTML-significant characters, so only `org_name` needs escaping. In-tree
/// per CLAUDE.md §8 — a five-char replacement is well under the dep bar.
fn html_escape(raw: &str) -> String {
    raw.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

/// Responsive, dark-mode-aware HTML invite — the authoritative copy of the
/// member-invite email. Table-based with inline styles for broad client
/// support (Gmail/Outlook/Apple Mail); the logo is a hosted PNG (clients
/// strip SVG / inline images). `include_str!` compiles the single on-disk
/// source in, so there is no second hand-maintained copy to drift — open
/// `src/orgs/invite-email-template.html` in a browser to preview. The three
/// `{{TOKENS}}` are substituted by [`render_invite`].
const INVITE_HTML_TEMPLATE: &str = include_str!("invite-email-template.html");

/// Render the invite into subject + plain-text + HTML parts. Pure: no I/O,
/// no clock, fully determined by `mail`.
fn render_invite(mail: &InviteMail<'_>) -> RenderedInvite {
    let url = accept_url(mail);
    let role = mail.role.as_str();
    let org = mail.org_name;
    let subject = format!("You're invited to join {org} on Patom");
    let text = format!(
        "You've been invited to join {org} on Patom as {role}.\n\n\
         Accept your invitation:\n{url}\n\n\
         This link expires in 7 days. If you weren't expecting this, you can \
         safely ignore this email."
    );
    // Fill the template. `org` is operator-controlled so it is HTML-escaped;
    // the URL (URL-safe token + regex-constrained slug + validated origin) and
    // role (closed enum) carry no HTML-significant characters. ORG_NAME is
    // substituted LAST so its (escaped) content can never re-expand a token.
    let html = INVITE_HTML_TEMPLATE
        .replace("{{ACCEPT_URL}}", &url)
        .replace("{{ROLE}}", role)
        .replace("{{ORG_NAME}}", &html_escape(org));
    RenderedInvite {
        subject,
        text,
        html,
    }
}

/// Parse an [`Email`] (already structurally validated at the config/HTTP
/// boundary) into a lettre [`Mailbox`], attaching an optional display name.
fn mailbox(email: &Email, name: Option<&str>) -> Result<Mailbox, MailError> {
    let address = email
        .as_str()
        .parse::<Address>()
        .map_err(|_| MailError::Address {
            raw: email.as_str().to_owned(),
        })?;
    Ok(Mailbox::new(name.map(ToOwned::to_owned), address))
}

/// Assemble the MIME message: `from` (with display name) → recipient, an
/// alternative text+HTML body. Factored out of `send_invite` so the header
/// wiring is testable via [`Message::formatted`] with no network.
fn build_message(from: &Mailbox, mail: &InviteMail<'_>) -> Result<Message, MailError> {
    let to = mailbox(mail.to, None)?;
    let rendered = render_invite(mail);
    Message::builder()
        .from(from.clone())
        .to(to)
        .subject(rendered.subject)
        .multipart(MultiPart::alternative_plain_html(
            rendered.text,
            rendered.html,
        ))
        .map_err(MailError::Build)
}

/// SMTP-backed mailer.
///
/// The transport (with its connection pool) is built once at startup from
/// [`SmtpSettings`] and reused across sends (CLAUDE.md §9). The `from`
/// mailbox is parsed once, here, for the same reason.
pub struct SmtpMailer {
    transport: AsyncSmtpTransport<Tokio1Executor>,
    from: Mailbox,
}

impl std::fmt::Debug for SmtpMailer {
    // The trait requires `Debug`, but the transport holds credentials and is
    // not itself `Debug`. Expose only the non-secret `from` mailbox and hide
    // the rest so a stray `?mailer` can never leak the relay password.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SmtpMailer")
            .field("from", &self.from)
            .finish_non_exhaustive()
    }
}

impl SmtpMailer {
    /// Build the relay transport from config. Fails fast (startup) on a bad
    /// `from` address or unbuildable TLS/relay setup.
    pub fn try_new(cfg: &SmtpSettings) -> Result<Self, MailError> {
        let from = mailbox(&cfg.from, cfg.from_name.as_deref())?;
        let credentials = Credentials::new(
            cfg.username.expose().to_owned(),
            cfg.password.expose().to_owned(),
        );
        // STARTTLS submission (RFC 6409 port 587) is the one transport mode
        // we support: it is universally accepted across popular relays (SES,
        // Postmark, Mailgun, SendGrid, Resend…), whereas implicit-TLS/465 is
        // not (e.g. Postmark has no 465). `starttls_relay` *requires* the TLS
        // upgrade — it never falls back to cleartext — so credentials and the
        // message are always encrypted in transit. `port` stays overridable
        // for a relay on a non-standard STARTTLS port, but the mode is fixed.
        let transport = AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(&cfg.host)?
            .port(cfg.port)
            .credentials(credentials)
            .build();
        Ok(Self { transport, from })
    }
}

#[async_trait]
impl Mailer for SmtpMailer {
    async fn send_invite(&self, mail: InviteMail<'_>) {
        // Build first; a render/header failure is a programmer/config error,
        // not a transient one, so log and bail without touching the network.
        let message = match build_message(&self.from, &mail) {
            Ok(message) => message,
            Err(e) => {
                tracing::error!(
                    event = "org.invite.send_failed",
                    patom.org.slug = %mail.org_slug,
                    patom.invite.role = %mail.role.as_str(),
                    error = ?e,
                );
                return;
            }
        };
        // CLAUDE.md §5: the I/O await is bounded. Fire-and-forget — a failure
        // here must not propagate (the invite row stands; the user can
        // resend), so every arm just emits a structured event.
        match tokio::time::timeout(EMAIL_SEND_TIMEOUT, self.transport.send(message)).await {
            Ok(Ok(_response)) => {
                tracing::info!(
                    event = "org.invite.sent",
                    patom.org.slug = %mail.org_slug,
                    patom.invite.role = %mail.role.as_str(),
                );
            }
            Ok(Err(e)) => {
                tracing::error!(
                    event = "org.invite.send_failed",
                    patom.org.slug = %mail.org_slug,
                    patom.invite.role = %mail.role.as_str(),
                    error = ?e,
                );
            }
            Err(_elapsed) => {
                tracing::error!(
                    event = "org.invite.send_timeout",
                    patom.org.slug = %mail.org_slug,
                    timeout_secs = EMAIL_SEND_TIMEOUT.as_secs(),
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // A 43-char unpadded-base64url string — the minimum valid `InviteToken`.
    const SAMPLE_TOKEN: &str = "abcdefghijklmnopqrstuvwxyz0123456789-_ABCDEF";

    fn email(raw: &str) -> Email {
        Email::try_from(raw).expect("valid email")
    }

    fn slug() -> OrgSlug {
        OrgSlug::try_from("acme").expect("valid slug")
    }

    fn token() -> InviteToken {
        InviteToken::try_from(SAMPLE_TOKEN).expect("valid token")
    }

    fn sample_mail<'a>(
        to: &'a Email,
        org_slug: &'a OrgSlug,
        tok: &'a InviteToken,
        web_base_url: Option<&'a str>,
    ) -> InviteMail<'a> {
        InviteMail {
            to,
            org_name: "Acme Inc",
            org_slug,
            role: Role::Member,
            token: tok,
            web_base_url,
        }
    }

    #[test]
    fn accept_url_uses_configured_base() {
        let to = email("dev@acme.test");
        let s = slug();
        let tok = token();
        let mail = sample_mail(&to, &s, &tok, Some("https://app.patom.app"));
        assert_eq!(
            accept_url(&mail),
            format!("https://app.patom.app/i/acme/{SAMPLE_TOKEN}")
        );
    }

    #[test]
    fn accept_url_falls_back_to_apex() {
        let to = email("dev@acme.test");
        let s = slug();
        let tok = token();
        let mail = sample_mail(&to, &s, &tok, None);
        assert_eq!(
            accept_url(&mail),
            format!("https://patom.app/i/acme/{SAMPLE_TOKEN}")
        );
    }

    #[test]
    fn accept_url_strips_trailing_slash_on_base() {
        let to = email("dev@acme.test");
        let s = slug();
        let tok = token();
        let mail = sample_mail(&to, &s, &tok, Some("https://app.patom.app/"));
        assert!(!accept_url(&mail).contains("//i/"));
    }

    #[test]
    fn render_carries_link_role_and_org_in_both_parts() {
        let to = email("dev@acme.test");
        let s = slug();
        let tok = token();
        let mail = sample_mail(&to, &s, &tok, Some("https://app.patom.app"));
        let r = render_invite(&mail);
        let url = accept_url(&mail);
        assert!(r.subject.contains("Acme Inc"));
        for body in [&r.text, &r.html] {
            assert!(body.contains(&url), "missing accept url in {body}");
            assert!(body.contains("member"), "missing role in {body}");
            assert!(body.contains("Acme Inc"), "missing org in {body}");
        }
    }

    #[test]
    fn render_html_includes_logo_and_cta_and_leaves_no_tokens() {
        let to = email("dev@acme.test");
        let s = slug();
        let tok = token();
        let mail = sample_mail(&to, &s, &tok, Some("https://app.patom.app"));
        let r = render_invite(&mail);
        assert!(
            r.html
                .contains("https://asset.patom.app/email/favicon-512.png"),
            "logo image missing"
        );
        assert!(r.html.contains("Accept invitation"), "CTA missing");
        // Every placeholder must be substituted — a stray token means a
        // template/render drift that would ship `{{...}}` to a recipient.
        assert!(!r.html.contains("{{"), "unsubstituted token in html");
    }

    #[test]
    fn render_escapes_html_in_org_name() {
        let to = email("dev@acme.test");
        let s = slug();
        let tok = token();
        let mut mail = sample_mail(&to, &s, &tok, None);
        mail.org_name = "<script>alert(1)</script>";
        let r = render_invite(&mail);
        // The text part is unescaped; the HTML part must neutralise the tag.
        assert!(!r.html.contains("<script>"));
        assert!(r.html.contains("&lt;script&gt;"));
    }

    #[test]
    fn build_message_sets_envelope_and_multipart() {
        let to = email("invitee@acme.test");
        let s = slug();
        let tok = token();
        let mail = sample_mail(&to, &s, &tok, Some("https://app.patom.app"));
        let from = mailbox(&email("invites@patom.app"), Some("Patom")).expect("from");
        let message = build_message(&from, &mail).expect("builds");
        let bytes = message.formatted();
        let rendered = String::from_utf8_lossy(&bytes);
        // Headers are emitted verbatim (not body-encoded), so assert on them.
        assert!(rendered.contains("invitee@acme.test"));
        assert!(rendered.contains("invites@patom.app"));
        assert!(rendered.contains("multipart/alternative"));
    }

    #[test]
    fn mailbox_rejects_malformed_address() {
        // `Email` is permissive ("missing @" is the only structural gate),
        // but lettre's `Address` is stricter — a space is rejected. Proves
        // the boundary surfaces a typed `MailError` rather than panicking.
        let bad = email("a b@acme.test");
        let err = mailbox(&bad, None).expect_err("should reject");
        assert!(matches!(err, MailError::Address { .. }));
    }
}
