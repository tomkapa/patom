//! Postgres-backed adapter that lets `rmcp::transport::auth::OAuthState`
//! persist PKCE/CSRF state across the browser callback hop.
//!
//! Rmcp's [`StateStore`] is single-keyed by CSRF token; patom's
//! `mcp_oauth_pending` table additionally carries per-flow tenant + UX
//! context (server_id, user_id, org_id, redirect_to, resume_ctx,
//! slack_ctx, expires_at) that rmcp doesn't model. The two write paths:
//!
//!   * **Start handler** — constructs `PatomStateStore::writer(...)`
//!     capturing the patom-side context, then drives
//!     `OAuthState::start_authorization` which calls `save(csrf, state)`.
//!     The adapter SQL inserts BOTH the patom context AND rmcp's
//!     `pkce_verifier`.
//!   * **Callback handler** — reads the patom-side context separately via
//!     [`PgMcpOAuthPendingStore::read_pending_ctx`], reconstitutes the
//!     `OAuthState` with a `PatomStateStore::reader(...)` (no write
//!     context), and calls `handle_callback(code, state)` which fires
//!     `state_store.load(state)` + `state_store.delete(state)`.

use std::fmt;
use std::sync::Arc;

use async_trait::async_trait;
use rmcp::transport::auth::{AuthError, StateStore, StoredAuthorizationState};
use sqlx::PgPool;

use crate::agents::AgentId;
use crate::auth::{OrgId, UserId};
use crate::clock::SharedClock;
use crate::mcp::McpServerId;
use crate::threads::ThreadId;

use super::errors::OAuthError;

/// Channel-agnostic resume context for the OAuth callback.
///
/// When populated on a [`PatomPendingCtx`], the callback appends a synthetic
/// continuation prompt ("I've connected <name>. Please continue.") into
/// `thread_id`'s feed and enqueues a trigger so the agent loop can resume
/// without the user typing anything. Set by any channel that drives the start
/// flow on behalf of an in-flight conversation (web UI, Slack adapter,
/// future Lark / Teams). Absent for manual "wire from the catalog
/// page" flows where there is no live conversation to resume.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResumeCtx {
    pub thread_id: ThreadId,
    pub agent_id: AgentId,
}

/// Slack-channel context for the "✓ Connected" follow-up ping.
///
/// Posted into the originating thread after the callback succeeds.
/// Slack-only — never populated for the web flow. Independent of
/// [`ResumeCtx`]: a Slack-initiated flow populates both; a manual
/// Slack-side wiring (hypothetical future "/patom connect notion"
/// command without an active thread) might populate this alone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlackPingCtx {
    pub team_id: String,
    pub channel_id: String,
    pub thread_ts: String,
}

/// Lark-channel context for the "✓ Connected" follow-up ping.
///
/// The Lark peer of [`SlackPingCtx`]. Posted into the originating chat
/// after the callback succeeds — `app_id` selects the bot whose
/// `tenant_access_token` posts, `chat_id` is the target chat, and
/// `reply_to` (optional) threads the ping under the triggering message.
/// Lark-only; never populated for the web or Slack flows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LarkPingCtx {
    pub app_id: String,
    pub chat_id: String,
    pub reply_to: Option<String>,
}

/// Discord-channel context for the "✓ Connected" follow-up ping.
///
/// The Discord peer of [`SlackPingCtx`]. `application_id` selects the
/// posting bot, `container_id` is the channel/thread, and `reply_to`
/// (optional) threads the ping under the triggering message. Discord-only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscordPingCtx {
    pub application_id: String,
    pub container_id: String,
    pub reply_to: Option<String>,
}

/// Patom-side context attached to a pending OAuth flow.
///
/// Owned by `PatomStateStore` (start path) or read out of the pending
/// row at callback time. Rmcp's `StoredAuthorizationState` (the
/// per-flow PKCE/CSRF blob) is persisted alongside but in separate
/// columns; the two are looked up by the same `state` primary key.
#[derive(Debug, Clone)]
pub struct PatomPendingCtx {
    pub server_id: McpServerId,
    pub user_id: UserId,
    pub org_id: OrgId,
    pub redirect_to: Option<String>,
    pub resume_ctx: Option<ResumeCtx>,
    pub slack_ctx: Option<SlackPingCtx>,
    pub lark_ctx: Option<LarkPingCtx>,
    pub discord_ctx: Option<DiscordPingCtx>,
    pub expires_at: chrono::DateTime<chrono::Utc>,
}

/// Shared handle to the Postgres-backed pending-row layer. Threaded
/// into the [`AppState`] and used to build per-flow adapters on each
/// `/oauth/start` and `/oauth/callback`.
pub struct PgMcpOAuthPendingStore {
    pool: PgPool,
    clock: SharedClock,
}

impl PgMcpOAuthPendingStore {
    #[must_use]
    pub fn new(pool: PgPool, clock: SharedClock) -> Self {
        Self { pool, clock }
    }

    /// Build a state-store adapter for the start-of-flow path.
    /// Subsequent `save(csrf, state)` calls from rmcp will INSERT a row
    /// with `ctx` populated alongside rmcp's `pkce_verifier`.
    #[must_use]
    pub fn writer(self: Arc<Self>, ctx: PatomPendingCtx) -> PatomStateStore {
        PatomStateStore {
            inner: self,
            write_ctx: Some(ctx),
        }
    }

    /// Build a state-store adapter for the callback path. `save` is not
    /// expected to be called on this adapter — only `load` and `delete`.
    #[must_use]
    pub fn reader(self: Arc<Self>) -> PatomStateStore {
        PatomStateStore {
            inner: self,
            write_ctx: None,
        }
    }

    /// Read the patom-side context attached to a pending flow without
    /// deleting the row. Rmcp's `handle_callback` will fire `state_store
    /// .delete(csrf)` inside `OAuthState::handle_callback`; we don't
    /// double-delete here.
    ///
    /// Returns `None` when the row is unknown or already past its TTL —
    /// the caller redirects with `status=state_expired` in either case.
    pub async fn read_pending_ctx(
        &self,
        csrf: &str,
    ) -> Result<Option<PatomPendingCtx>, OAuthError> {
        let now = self.clock.now_utc();
        let row = crate::auth::run_privileged::<Option<PendingCtxRow>, OAuthError>(
            &self.pool,
            async |tx| {
                Ok(sqlx::query_as::<_, PendingCtxRow>(
                    "SELECT server_id, user_id, org_id, redirect_to, expires_at, \
                            thread_id, agent_id, \
                            slack_team_id, slack_channel_id, slack_thread_ts, \
                            lark_app_id, lark_chat_id, lark_reply_to, \
                            discord_application_id, discord_container_id, discord_reply_to \
                       FROM mcp_oauth_pending \
                      WHERE state = $1 AND expires_at > $2",
                )
                .bind(csrf)
                .bind(now)
                .fetch_optional(&mut **tx)
                .await?)
            },
        )
        .await?;
        row.map(PendingCtxRow::into_ctx).transpose()
    }
}

impl fmt::Debug for PgMcpOAuthPendingStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PgMcpOAuthPendingStore")
            .finish_non_exhaustive()
    }
}

/// `Arc`-wrapped handle used by the AppState.
pub type SharedMcpOAuthPendingStore = Arc<PgMcpOAuthPendingStore>;

/// Per-flow adapter handed to rmcp's `AuthorizationManager` via
/// `set_state_store`. Bound at construction to either the start
/// path (`writer`) or the callback path (`reader`).
pub struct PatomStateStore {
    inner: Arc<PgMcpOAuthPendingStore>,
    write_ctx: Option<PatomPendingCtx>,
}

impl fmt::Debug for PatomStateStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PatomStateStore")
            .field("has_write_ctx", &self.write_ctx.is_some())
            .finish_non_exhaustive()
    }
}

fn pending_err(prefix: &'static str, e: OAuthError) -> AuthError {
    AuthError::InternalError(format!("{prefix}: {e}"))
}

#[async_trait]
impl StateStore for PatomStateStore {
    async fn save(&self, csrf: &str, state: StoredAuthorizationState) -> Result<(), AuthError> {
        let ctx = self.write_ctx.as_ref().ok_or_else(|| {
            AuthError::InternalError(
                "PatomStateStore::save called without write context — callback-path \
                 reader was used on the start path"
                    .into(),
            )
        })?;
        let now = self.inner.clock.now_utc();
        let pool = &self.inner.pool;
        let ctx = ctx.clone();
        let pkce_verifier = state.pkce_verifier;
        let csrf = csrf.to_owned();
        crate::auth::run_privileged::<(), OAuthError>(pool, async |tx| {
            sqlx::query(
                "INSERT INTO mcp_oauth_pending \
                 (state, server_id, user_id, org_id, pkce_verifier, redirect_to, \
                  created_at, expires_at, thread_id, agent_id, \
                  slack_team_id, slack_channel_id, slack_thread_ts, \
                  lark_app_id, lark_chat_id, lark_reply_to, \
                  discord_application_id, discord_container_id, discord_reply_to) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, \
                         $14, $15, $16, $17, $18, $19)",
            )
            .bind(&csrf)
            .bind(ctx.server_id)
            .bind(ctx.user_id)
            .bind(ctx.org_id)
            .bind(&pkce_verifier)
            .bind(ctx.redirect_to.as_deref())
            .bind(now)
            .bind(ctx.expires_at)
            .bind(ctx.resume_ctx.map(|r| r.thread_id))
            .bind(ctx.resume_ctx.map(|r| r.agent_id))
            .bind(ctx.slack_ctx.as_ref().map(|s| s.team_id.as_str()))
            .bind(ctx.slack_ctx.as_ref().map(|s| s.channel_id.as_str()))
            .bind(ctx.slack_ctx.as_ref().map(|s| s.thread_ts.as_str()))
            .bind(ctx.lark_ctx.as_ref().map(|l| l.app_id.as_str()))
            .bind(ctx.lark_ctx.as_ref().map(|l| l.chat_id.as_str()))
            .bind(ctx.lark_ctx.as_ref().and_then(|l| l.reply_to.as_deref()))
            .bind(ctx.discord_ctx.as_ref().map(|d| d.application_id.as_str()))
            .bind(ctx.discord_ctx.as_ref().map(|d| d.container_id.as_str()))
            .bind(ctx.discord_ctx.as_ref().and_then(|d| d.reply_to.as_deref()))
            .execute(&mut **tx)
            .await?;
            Ok(())
        })
        .await
        .map_err(|e| pending_err("pending insert", e))
    }

    async fn load(&self, csrf: &str) -> Result<Option<StoredAuthorizationState>, AuthError> {
        let pool = &self.inner.pool;
        let csrf_str = csrf.to_owned();
        let row =
            crate::auth::run_privileged::<Option<PendingPkceRow>, OAuthError>(pool, async |tx| {
                Ok(sqlx::query_as::<_, PendingPkceRow>(
                    "SELECT pkce_verifier \
                       FROM mcp_oauth_pending \
                      WHERE state = $1",
                )
                .bind(&csrf_str)
                .fetch_optional(&mut **tx)
                .await?)
            })
            .await
            .map_err(|e| pending_err("pending load", e))?;
        Ok(row.map(|r| {
            // `StoredAuthorizationState` is `#[non_exhaustive]` in
            // rmcp; use the published constructor. `created_at` is
            // stamped from the system clock inside `new` — that's a
            // small fidelity loss versus the original timestamp, but
            // patom's own `expires_at` CHECK on `mcp_oauth_pending`
            // already filters stale rows before they reach here, so
            // rmcp's internal TTL is redundant.
            use oauth2::{CsrfToken, PkceCodeVerifier};
            StoredAuthorizationState::new(
                &PkceCodeVerifier::new(r.pkce_verifier),
                &CsrfToken::new(csrf_str.clone()),
            )
        }))
    }

    async fn delete(&self, csrf: &str) -> Result<(), AuthError> {
        let pool = &self.inner.pool;
        let csrf = csrf.to_owned();
        crate::auth::run_privileged::<(), OAuthError>(pool, async |tx| {
            sqlx::query("DELETE FROM mcp_oauth_pending WHERE state = $1")
                .bind(&csrf)
                .execute(&mut **tx)
                .await?;
            Ok(())
        })
        .await
        .map_err(|e| pending_err("pending delete", e))
    }
}

#[derive(sqlx::FromRow)]
struct PendingCtxRow {
    server_id: McpServerId,
    user_id: UserId,
    org_id: OrgId,
    redirect_to: Option<String>,
    expires_at: chrono::DateTime<chrono::Utc>,
    thread_id: Option<ThreadId>,
    agent_id: Option<AgentId>,
    slack_team_id: Option<String>,
    slack_channel_id: Option<String>,
    slack_thread_ts: Option<String>,
    lark_app_id: Option<String>,
    lark_chat_id: Option<String>,
    lark_reply_to: Option<String>,
    discord_application_id: Option<String>,
    discord_container_id: Option<String>,
    discord_reply_to: Option<String>,
}

impl PendingCtxRow {
    /// Decode a fetched row into the boundary type. The all-or-none
    /// CHECK constraints on `mcp_oauth_pending` keep each tuple uniform,
    /// so half-populated reads should be unreachable — but a future
    /// constraint regression or an out-of-band `INSERT` mustn't crash
    /// the process (CLAUDE.md §12 forbids `panic!` across a module
    /// boundary; the previous `panic = abort` would SIGABRT a
    /// recoverable user-visible failure). Return a typed
    /// `Misconfigured` error instead so the callback handler can
    /// surface a clean redirect.
    fn into_ctx(self) -> Result<PatomPendingCtx, OAuthError> {
        let resume_ctx = match (self.thread_id, self.agent_id) {
            (Some(thread_id), Some(agent_id)) => Some(ResumeCtx {
                thread_id,
                agent_id,
            }),
            (None, None) => None,
            _ => {
                return Err(OAuthError::Misconfigured(
                    "mcp_oauth_pending.resume_ctx half-populated; \
                     CHECK constraint violated"
                        .to_owned(),
                ));
            }
        };
        let slack_ctx = match (
            self.slack_team_id,
            self.slack_channel_id,
            self.slack_thread_ts,
        ) {
            (Some(team_id), Some(channel_id), Some(thread_ts)) => Some(SlackPingCtx {
                team_id,
                channel_id,
                thread_ts,
            }),
            (None, None, None) => None,
            _ => {
                return Err(OAuthError::Misconfigured(
                    "mcp_oauth_pending.slack_ctx partially populated; \
                     CHECK constraint violated"
                        .to_owned(),
                ));
            }
        };
        let lark_ctx = decode_lark_ctx(self.lark_app_id, self.lark_chat_id, self.lark_reply_to)?;
        let discord_ctx = decode_discord_ctx(
            self.discord_application_id,
            self.discord_container_id,
            self.discord_reply_to,
        )?;
        Ok(PatomPendingCtx {
            server_id: self.server_id,
            user_id: self.user_id,
            org_id: self.org_id,
            redirect_to: self.redirect_to,
            resume_ctx,
            slack_ctx,
            lark_ctx,
            discord_ctx,
            expires_at: self.expires_at,
        })
    }
}

/// Decode the Lark ping-context column group into [`LarkPingCtx`], enforcing
/// the all-or-none invariant the DB CHECK guarantees. A reply anchor without
/// its chat target is meaningless — defend the decode so a stray row (out-of-
/// band INSERT / future CHECK regression) can't silently drop the `reply_to`.
fn decode_lark_ctx(
    app_id: Option<String>,
    chat_id: Option<String>,
    reply_to: Option<String>,
) -> Result<Option<LarkPingCtx>, OAuthError> {
    match (app_id, chat_id) {
        (Some(app_id), Some(chat_id)) => Ok(Some(LarkPingCtx {
            app_id,
            chat_id,
            reply_to,
        })),
        (None, None) if reply_to.is_none() => Ok(None),
        _ => Err(OAuthError::Misconfigured(
            "mcp_oauth_pending.lark_ctx partially populated; \
             CHECK constraint violated"
                .to_owned(),
        )),
    }
}

/// Decode the Discord ping-context column group into [`DiscordPingCtx`].
/// Peer of [`decode_lark_ctx`].
fn decode_discord_ctx(
    application_id: Option<String>,
    container_id: Option<String>,
    reply_to: Option<String>,
) -> Result<Option<DiscordPingCtx>, OAuthError> {
    match (application_id, container_id) {
        (Some(application_id), Some(container_id)) => Ok(Some(DiscordPingCtx {
            application_id,
            container_id,
            reply_to,
        })),
        (None, None) if reply_to.is_none() => Ok(None),
        _ => Err(OAuthError::Misconfigured(
            "mcp_oauth_pending.discord_ctx partially populated; \
             CHECK constraint violated"
                .to_owned(),
        )),
    }
}

#[derive(sqlx::FromRow)]
struct PendingPkceRow {
    pkce_verifier: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `PendingCtxRow` with required columns set and every optional
    /// context group left empty. Tests flip individual fields to exercise
    /// the all-or-none decode.
    fn base_row() -> PendingCtxRow {
        PendingCtxRow {
            server_id: McpServerId::new(),
            user_id: UserId::new(),
            org_id: OrgId::new(),
            redirect_to: None,
            expires_at: chrono::DateTime::<chrono::Utc>::MIN_UTC,
            thread_id: None,
            agent_id: None,
            slack_team_id: None,
            slack_channel_id: None,
            slack_thread_ts: None,
            lark_app_id: None,
            lark_chat_id: None,
            lark_reply_to: None,
            discord_application_id: None,
            discord_container_id: None,
            discord_reply_to: None,
        }
    }

    #[test]
    fn lark_ctx_round_trips_with_reply_to() {
        let row = PendingCtxRow {
            lark_app_id: Some("cli_app".to_owned()),
            lark_chat_id: Some("oc_chat".to_owned()),
            lark_reply_to: Some("om_msg".to_owned()),
            ..base_row()
        };
        let ctx = row.into_ctx().expect("decodes");
        assert_eq!(
            ctx.lark_ctx,
            Some(LarkPingCtx {
                app_id: "cli_app".to_owned(),
                chat_id: "oc_chat".to_owned(),
                reply_to: Some("om_msg".to_owned()),
            })
        );
        assert!(ctx.discord_ctx.is_none());
    }

    #[test]
    fn lark_ctx_round_trips_without_reply_to() {
        let row = PendingCtxRow {
            lark_app_id: Some("cli_app".to_owned()),
            lark_chat_id: Some("oc_chat".to_owned()),
            ..base_row()
        };
        let ctx = row.into_ctx().expect("decodes");
        assert_eq!(
            ctx.lark_ctx,
            Some(LarkPingCtx {
                app_id: "cli_app".to_owned(),
                chat_id: "oc_chat".to_owned(),
                reply_to: None,
            })
        );
    }

    #[test]
    fn discord_ctx_round_trips() {
        let row = PendingCtxRow {
            discord_application_id: Some("123".to_owned()),
            discord_container_id: Some("456".to_owned()),
            discord_reply_to: Some("789".to_owned()),
            ..base_row()
        };
        let ctx = row.into_ctx().expect("decodes");
        assert_eq!(
            ctx.discord_ctx,
            Some(DiscordPingCtx {
                application_id: "123".to_owned(),
                container_id: "456".to_owned(),
                reply_to: Some("789".to_owned()),
            })
        );
        assert!(ctx.lark_ctx.is_none());
    }

    #[test]
    fn half_populated_lark_ctx_is_misconfigured() {
        let row = PendingCtxRow {
            lark_app_id: Some("cli_app".to_owned()),
            // chat_id missing — CHECK regression / out-of-band insert.
            ..base_row()
        };
        assert!(matches!(row.into_ctx(), Err(OAuthError::Misconfigured(_))));
    }

    #[test]
    fn half_populated_discord_ctx_is_misconfigured() {
        let row = PendingCtxRow {
            discord_container_id: Some("456".to_owned()),
            // application_id missing.
            ..base_row()
        };
        assert!(matches!(row.into_ctx(), Err(OAuthError::Misconfigured(_))));
    }

    #[test]
    fn lark_reply_to_without_pair_is_misconfigured() {
        // The DB CHECK forbids this, but defend the decode too.
        let row = PendingCtxRow {
            lark_reply_to: Some("om_msg".to_owned()),
            ..base_row()
        };
        assert!(matches!(row.into_ctx(), Err(OAuthError::Misconfigured(_))));
    }
}
