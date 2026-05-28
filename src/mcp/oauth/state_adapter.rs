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
use crate::session::SessionId;

use super::errors::OAuthError;

/// Channel-agnostic resume context for the OAuth callback.
///
/// When populated on a [`PatomPendingCtx`], the callback enqueues a
/// synthetic continuation prompt ("I've connected <name>. Please
/// continue.") into `session_id` so the agent loop can resume without
/// the user typing anything. Set by any channel that drives the start
/// flow on behalf of an in-flight conversation (web UI, Slack adapter,
/// future Lark / Teams). Absent for manual "wire from the catalog
/// page" flows where there is no live conversation to resume.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResumeCtx {
    pub session_id: SessionId,
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
                            session_id, agent_id, \
                            slack_team_id, slack_channel_id, slack_thread_ts \
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
        Ok(row.map(PendingCtxRow::into_ctx))
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
                  created_at, expires_at, session_id, agent_id, \
                  slack_team_id, slack_channel_id, slack_thread_ts) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13)",
            )
            .bind(&csrf)
            .bind(ctx.server_id)
            .bind(ctx.user_id)
            .bind(ctx.org_id)
            .bind(&pkce_verifier)
            .bind(ctx.redirect_to.as_deref())
            .bind(now)
            .bind(ctx.expires_at)
            .bind(ctx.resume_ctx.map(|r| r.session_id))
            .bind(ctx.resume_ctx.map(|r| r.agent_id))
            .bind(ctx.slack_ctx.as_ref().map(|s| s.team_id.as_str()))
            .bind(ctx.slack_ctx.as_ref().map(|s| s.channel_id.as_str()))
            .bind(ctx.slack_ctx.as_ref().map(|s| s.thread_ts.as_str()))
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
    session_id: Option<SessionId>,
    agent_id: Option<AgentId>,
    slack_team_id: Option<String>,
    slack_channel_id: Option<String>,
    slack_thread_ts: Option<String>,
}

impl PendingCtxRow {
    fn into_ctx(self) -> PatomPendingCtx {
        // The all-or-none CHECK constraints on `mcp_oauth_pending` keep
        // each tuple uniform; a half-populated read is a schema-vs-code
        // divergence that should crash the process (CLAUDE.md §6).
        let resume_ctx = match (self.session_id, self.agent_id) {
            (Some(session_id), Some(agent_id)) => Some(ResumeCtx {
                session_id,
                agent_id,
            }),
            (None, None) => None,
            _ => panic!(
                "invariant: mcp_oauth_pending.resume_ctx half-populated; \
                 CHECK constraint violated"
            ),
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
            _ => panic!(
                "invariant: mcp_oauth_pending.slack_ctx partially-populated; \
                 CHECK constraint violated"
            ),
        };
        PatomPendingCtx {
            server_id: self.server_id,
            user_id: self.user_id,
            org_id: self.org_id,
            redirect_to: self.redirect_to,
            resume_ctx,
            slack_ctx,
            expires_at: self.expires_at,
        }
    }
}

#[derive(sqlx::FromRow)]
struct PendingPkceRow {
    pkce_verifier: String,
}
