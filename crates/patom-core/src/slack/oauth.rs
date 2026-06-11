//! Slack v2 OAuth install + callback.
//!
//! Two routes:
//! - `POST /api/slack/install` (private; requires a signed-in Patom
//!   user). Returns `{ authorize_url }` for the SPA to open. The state
//!   parameter is a short-lived HMAC over `(org_id, user_id, exp)`
//!   keyed on the Slack signing secret — keeps state self-contained
//!   without a pending-row table (Phase 2 may add one for revocation).
//! - `GET /slack/oauth/callback` (public; Slack returns the user
//!   here). Validates the state, exchanges `code` via
//!   `https://slack.com/api/oauth.v2.access`, seals the bot token, and
//!   upserts `slack_workspaces`.
//!
//! The install also auto-links the installer's identity from the
//! `authed_user.id` Slack returns ([`LinkedVia::Installer`]), so the
//! workspace owner is never prompted by `/patom`. Per-user identity
//! linking for everyone else runs through the `/patom`-gated login flow
//! in [`super::identity_routes`] (GitHub issue #41).

use std::time::Duration;

use axum::Json;
use axum::Router;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{delete, get, post};
use chrono::{DateTime, Utc};
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use subtle::ConstantTimeEq;
use tracing::{error, info, warn};
use url::Url;

use crate::auth::{OrgId, Principal, UserId};
use crate::http::AppState;
use crate::http::HttpError;

use super::error::SlackError;
use super::identity::LinkedVia;
use super::state::SlackAppState;
use super::types::{SlackBotToken, SlackTeamId, SlackUserId};
use super::workspace::{NewWorkspace, WorkspaceSummary};

type HmacSha256 = Hmac<Sha256>;

/// Scopes requested at install.
///
/// - `app_mentions:read` — receive `app_mention` events.
/// - `chat:write` — post replies via `chat.postMessage`.
/// - `chat:write.customize` — override `username` + `icon_url` per
///   message (Phase 1 uses `username` only; `icon_url` is issue #43).
/// - `channels:history` — receive `message.channels` events for the
///   bot's invited channels so that untagged in-thread replies route
///   to the agent already bound in `slack_threads`. The bot only acts
///   on messages whose `thread_ts` is in `slack_threads`; everything
///   else is dropped at the boundary.
/// - `commands` — register the `/patom` slash command.
/// - `users:read` — call `users.info` to resolve the slash command
///   sender's workspace `display_name` and avatar so the synthetic
///   prompt-mirror post reads as the human, not the app default.
const SLACK_SCOPES: &str =
    "app_mentions:read,channels:history,chat:write,chat:write.customize,commands,users:read";

/// State token lifetime — long enough for a user to complete the
/// Slack consent screen, short enough that a leaked state is useless.
const STATE_TTL: Duration = Duration::from_mins(10);

pub fn private_router() -> Router<AppState> {
    Router::new()
        .route("/slack/install", post(install))
        .route("/slack/workspaces", get(list_workspaces))
        .route("/slack/workspaces/{team_id}", delete(disconnect_workspace))
}

pub fn public_router() -> Router<AppState> {
    Router::new().route("/slack/oauth/callback", get(callback))
}

// ────────────────────────────────────────────────────────────────────
// POST /api/slack/install
// ────────────────────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
struct InstallResponse {
    authorize_url: String,
}

async fn install(
    State(state): State<AppState>,
    principal: Principal,
) -> Result<Json<InstallResponse>, HttpError> {
    let slack = state.slack.as_ref().ok_or(HttpError::NotFound)?;
    let state_token = sign_state(
        slack.signing_secret.expose().as_bytes(),
        principal.active_org_id,
        principal.user_id,
        slack.clock.now_unix_secs() + i64::try_from(STATE_TTL.as_secs()).unwrap_or(600),
    );
    let mut url =
        Url::parse("https://slack.com/oauth/v2/authorize").map_err(|_| HttpError::Internal)?;
    url.query_pairs_mut()
        .append_pair("client_id", &slack.client_id)
        .append_pair("scope", SLACK_SCOPES)
        .append_pair("redirect_uri", &slack.redirect_url)
        .append_pair("state", &state_token);
    Ok(Json(InstallResponse {
        authorize_url: url.into(),
    }))
}

// ────────────────────────────────────────────────────────────────────
// GET /api/slack/workspaces
// ────────────────────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
struct WorkspaceSummaryDto {
    team_id: String,
    team_name: String,
    scopes: String,
    installed_by_user_id: UserId,
    installed_at: DateTime<Utc>,
}

impl From<WorkspaceSummary> for WorkspaceSummaryDto {
    fn from(s: WorkspaceSummary) -> Self {
        Self {
            team_id: s.team_id.as_str().to_owned(),
            team_name: s.team_name,
            scopes: s.scopes,
            installed_by_user_id: s.installed_by_user_id,
            installed_at: s.installed_at,
        }
    }
}

async fn list_workspaces(
    State(state): State<AppState>,
    principal: Principal,
) -> Result<Json<Vec<WorkspaceSummaryDto>>, HttpError> {
    let slack = state.slack.as_ref().ok_or(HttpError::NotFound)?;
    let rows = slack
        .workspaces
        .list(&principal)
        .await
        .map_err(slack_to_http)?;
    Ok(Json(
        rows.into_iter().map(WorkspaceSummaryDto::from).collect(),
    ))
}

// ────────────────────────────────────────────────────────────────────
// DELETE /api/slack/workspaces/{team_id}
// ────────────────────────────────────────────────────────────────────

async fn disconnect_workspace(
    State(state): State<AppState>,
    principal: Principal,
    Path(team_id): Path<String>,
) -> Result<StatusCode, HttpError> {
    let slack = state.slack.as_ref().ok_or(HttpError::NotFound)?;
    let team_id = SlackTeamId::try_from(team_id.as_str()).map_err(HttpError::Parse)?;
    slack
        .workspaces
        .delete(&principal, &team_id)
        .await
        .map_err(slack_to_http)?;
    Ok(StatusCode::NO_CONTENT)
}

fn slack_to_http(e: SlackError) -> HttpError {
    match e {
        SlackError::UnknownWorkspace(_) => HttpError::NotFound,
        SlackError::Parse(p) => HttpError::Parse(p),
        other => {
            error!(error = ?other, event = "slack.workspace.store_error");
            HttpError::Internal
        }
    }
}

// ────────────────────────────────────────────────────────────────────
// GET /slack/oauth/callback
// ────────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct CallbackQuery {
    #[serde(default)]
    code: Option<String>,
    #[serde(default)]
    state: Option<String>,
    #[serde(default)]
    error: Option<String>,
}

async fn callback(State(state): State<AppState>, Query(params): Query<CallbackQuery>) -> Response {
    let Some(slack) = state.slack.as_ref() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if let Some(err) = params.error {
        warn!(slack.error = %err, event = "slack.oauth.callback_error");
        return redirect_to_fe(state.web_base_url.as_deref(), Some(&err));
    }
    let Some(code) = params.code else {
        warn!(event = "slack.oauth.callback_missing_code");
        return StatusCode::BAD_REQUEST.into_response();
    };
    let Some(state_token) = params.state else {
        warn!(event = "slack.oauth.callback_missing_state");
        return StatusCode::BAD_REQUEST.into_response();
    };
    let Some(parsed) = verify_state(
        slack.signing_secret.expose().as_bytes(),
        &state_token,
        slack.clock.now_unix_secs(),
    ) else {
        warn!(event = "slack.oauth.callback_bad_state");
        return StatusCode::BAD_REQUEST.into_response();
    };

    // Exchange code → bot token.
    let exchange = match exchange_code(
        &slack.http,
        &slack.client_id,
        slack.client_secret.expose(),
        &code,
        &slack.redirect_url,
    )
    .await
    {
        Ok(e) => e,
        Err(e) => {
            error!(error = ?e, event = "slack.oauth.exchange_failed");
            return redirect_to_fe(state.web_base_url.as_deref(), Some("exchange_failed"));
        }
    };
    let Some(bot_token_str) = exchange.access_token else {
        warn!(event = "slack.oauth.no_access_token");
        return redirect_to_fe(state.web_base_url.as_deref(), Some("no_access_token"));
    };
    let bot_token = match SlackBotToken::try_from(bot_token_str) {
        Ok(t) => t,
        Err(e) => {
            warn!(error = %e, event = "slack.oauth.bad_token_shape");
            return StatusCode::BAD_GATEWAY.into_response();
        }
    };
    let team_id = match SlackTeamId::try_from(exchange.team.id.as_str()) {
        Ok(t) => t,
        Err(e) => {
            warn!(error = %e, event = "slack.oauth.bad_team_id");
            return StatusCode::BAD_GATEWAY.into_response();
        }
    };
    let bot_user_id = match SlackUserId::try_from(exchange.bot_user_id.as_str()) {
        Ok(u) => u,
        Err(e) => {
            warn!(error = %e, event = "slack.oauth.bad_bot_user_id");
            return StatusCode::BAD_GATEWAY.into_response();
        }
    };

    // Mint a Principal-ish for the installer so the `upsert` call
    // satisfies RLS via `begin_as_user`.
    let principal = Principal {
        user_id: parsed.user_id,
        active_org_id: parsed.org_id,
        // Role doesn't affect the workspace upsert RLS gate (membership
        // is the gate); use the existing default.
        role: crate::auth::Role::Member,
    };
    // Clone the bot token before it moves into the workspace row — the
    // installer auto-link uses it to fetch the installer's Slack display
    // name via `users.info`.
    let bot_token_for_sync = bot_token.clone();
    let new = NewWorkspace {
        org_id: parsed.org_id,
        team_id: team_id.clone(),
        team_name: exchange.team.name.unwrap_or_default(),
        bot_user_id,
        bot_token,
        scopes: exchange.scope.unwrap_or_else(|| SLACK_SCOPES.to_owned()),
        installed_by_user_id: parsed.user_id,
    };
    if let Err(e) = slack.workspaces.upsert(&principal, new).await {
        error!(error = ?e, event = "slack.oauth.workspace_upsert_failed");
        return redirect_to_fe(state.web_base_url.as_deref(), Some("install_failed"));
    }
    info!(
        patom.org.id = %parsed.org_id.as_uuid(),
        patom.user.id = %parsed.user_id.as_uuid(),
        event = "slack.oauth.installed",
    );
    // Auto-link the installer so the owner is never prompted by `/patom`.
    // Best-effort + non-fatal: the install already succeeded, so a link
    // failure must not fail the callback (the owner can still link via
    // `/patom`). Runs after the workspace upsert so the composite FK
    // `slack_identities(org_id, team_id) → slack_workspaces` is satisfied.
    link_installer_identity(
        slack,
        &bot_token_for_sync,
        &team_id,
        parsed.org_id,
        parsed.user_id,
        exchange.authed_user.as_ref(),
    )
    .await;
    redirect_to_fe(state.web_base_url.as_deref(), None)
}

/// Write the installer's `slack_identities` row from the OAuth
/// `authed_user.id`. No-op (with a log) when the field is absent or
/// malformed — installer auto-link is a convenience, not a correctness
/// requirement.
async fn link_installer_identity(
    slack: &SlackAppState,
    bot_token: &SlackBotToken,
    team_id: &SlackTeamId,
    org_id: OrgId,
    user_id: UserId,
    authed_user: Option<&AuthedUser>,
) {
    let Some(authed) = authed_user.filter(|a| !a.id.is_empty()) else {
        warn!(event = "slack.oauth.no_authed_user");
        return;
    };
    let installer_slack_user = match SlackUserId::try_from(authed.id.as_str()) {
        Ok(u) => u,
        Err(e) => {
            warn!(error = %e, event = "slack.oauth.bad_authed_user_id");
            return;
        }
    };
    match slack
        .identities
        .link_with_org(
            user_id,
            org_id,
            team_id,
            &installer_slack_user,
            LinkedVia::Installer,
        )
        .await
    {
        Ok(()) => info!(event = "slack.oauth.installer_linked"),
        Err(e) => {
            warn!(error = ?e, event = "slack.oauth.installer_link_failed");
            return;
        }
    }
    // Capture the installer's Slack handle as their per-platform label.
    if let Some(name) =
        crate::slack::bridge::fetch_slack_display_name(&slack.http, bot_token, authed.id.as_str())
            .await
        && let Err(e) = slack
            .identities
            .set_display_name(team_id, &installer_slack_user, &name)
            .await
    {
        warn!(error = ?e, event = "slack.oauth.installer_display_name_store_failed");
    }
}

fn redirect_to_fe(web_base: Option<&str>, error: Option<&str>) -> Response {
    let path = "/settings/integrations";
    let target = match (web_base, error) {
        (Some(base), Some(err)) => {
            let mut u = format!("{base}{path}");
            let mut sep = '?';
            u.push(sep);
            sep = '&';
            let _ = sep;
            let mut serializer = url::form_urlencoded::Serializer::new(String::new());
            serializer.append_pair("error", err);
            u.push_str(&serializer.finish());
            u
        }
        (Some(base), None) => format!("{base}{path}"),
        (None, Some(err)) => {
            let mut serializer = url::form_urlencoded::Serializer::new(String::new());
            serializer.append_pair("error", err);
            format!("{path}?{}", serializer.finish())
        }
        (None, None) => path.to_owned(),
    };
    Redirect::to(&target).into_response()
}

// ────────────────────────────────────────────────────────────────────
// State signing (HMAC over `<org>.<user>.<exp>`).
// ────────────────────────────────────────────────────────────────────

#[derive(Debug)]
struct StateClaims {
    org_id: OrgId,
    user_id: UserId,
}

fn sign_state(key: &[u8], org_id: OrgId, user_id: UserId, exp_secs: i64) -> String {
    let payload = format!("{}.{}.{exp_secs}", org_id.as_uuid(), user_id.as_uuid());
    let mut mac = HmacSha256::new_from_slice(key).expect("invariant: signing secret non-empty");
    mac.update(payload.as_bytes());
    let hex_sig = super::hex::encode_32(&mac.finalize().into_bytes());
    format!("{payload}.{hex_sig}")
}

fn verify_state(key: &[u8], token: &str, now_secs: i64) -> Option<StateClaims> {
    // Split into (payload, sig).
    let last_dot = token.rfind('.')?;
    let (payload, sig_with_dot) = token.split_at(last_dot);
    let sig_hex = &sig_with_dot[1..];
    if sig_hex.len() != 64 {
        return None;
    }

    // Recompute MAC.
    let mut mac = HmacSha256::new_from_slice(key).ok()?;
    mac.update(payload.as_bytes());
    let computed = mac.finalize().into_bytes();
    let mut expected = [0u8; 32];
    super::hex::decode_32(sig_hex, &mut expected).ok()?;
    if !bool::from(computed.ct_eq(&expected)) {
        return None;
    }

    // Parse payload `<org>.<user>.<exp>`.
    let mut parts = payload.split('.');
    let org_raw = parts.next()?;
    let user_raw = parts.next()?;
    let exp_raw = parts.next()?;
    if parts.next().is_some() {
        return None;
    }
    let org_id = uuid::Uuid::parse_str(org_raw).ok().map(OrgId::from)?;
    let user_id = uuid::Uuid::parse_str(user_raw).ok().map(UserId::from)?;
    let exp_secs: i64 = exp_raw.parse().ok()?;
    if exp_secs < now_secs {
        return None;
    }
    Some(StateClaims { org_id, user_id })
}

// ────────────────────────────────────────────────────────────────────
// oauth.v2.access exchange
// ────────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct ExchangeResponse {
    #[allow(dead_code)]
    ok: bool,
    #[serde(default)]
    access_token: Option<String>,
    #[serde(default)]
    bot_user_id: String,
    #[serde(default)]
    scope: Option<String>,
    team: ExchangeTeam,
    /// The Slack user who performed the install. Carries their Slack user
    /// id so we can auto-link the installer's identity (issue #41) — the
    /// owner is then never prompted by `/patom`.
    #[serde(default)]
    authed_user: Option<AuthedUser>,
}

#[derive(Debug, Deserialize)]
struct ExchangeTeam {
    id: String,
    #[serde(default)]
    name: Option<String>,
}

#[derive(Debug, Deserialize)]
struct AuthedUser {
    #[serde(default)]
    id: String,
}

async fn exchange_code(
    http: &reqwest::Client,
    client_id: &str,
    client_secret: &str,
    code: &str,
    redirect_url: &str,
) -> Result<ExchangeResponse, ExchangeError> {
    // Hand-build the urlencoded body — `reqwest::form` is gated behind
    // a feature we don't enable, and the body is four fields.
    let body = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("client_id", client_id)
        .append_pair("client_secret", client_secret)
        .append_pair("code", code)
        .append_pair("redirect_uri", redirect_url)
        .finish();
    let resp = http
        .post("https://slack.com/api/oauth.v2.access")
        .header("content-type", "application/x-www-form-urlencoded")
        .body(body)
        .send()
        .await
        .map_err(ExchangeError::Transport)?;
    if !resp.status().is_success() {
        return Err(ExchangeError::Status(resp.status().as_u16()));
    }
    let parsed: ExchangeResponse = resp.json().await.map_err(ExchangeError::Decode)?;
    if !parsed.ok {
        return Err(ExchangeError::SlackNotOk);
    }
    Ok(parsed)
}

#[derive(Debug, thiserror::Error)]
enum ExchangeError {
    #[error("transport: {0}")]
    Transport(reqwest::Error),
    #[error("non-2xx: {0}")]
    Status(u16),
    #[error("decode: {0}")]
    Decode(reqwest::Error),
    #[error("oauth.v2.access ok=false")]
    SlackNotOk,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_roundtrip_succeeds() {
        let key = b"a-test-key-32-bytes-or-more-aaaaaaaaaaaa";
        let org = OrgId::new();
        let user = UserId::new();
        let exp = 4_102_444_800; // year 2100
        let token = sign_state(key, org, user, exp);
        let parsed = verify_state(key, &token, 1_700_000_000).expect("valid");
        assert_eq!(parsed.org_id, org);
        assert_eq!(parsed.user_id, user);
    }

    #[test]
    fn state_rejects_expired() {
        let key = b"a-test-key-32-bytes-or-more-aaaaaaaaaaaa";
        let org = OrgId::new();
        let user = UserId::new();
        let exp = 1_700_000_000;
        let token = sign_state(key, org, user, exp);
        assert!(verify_state(key, &token, exp + 1).is_none());
    }

    #[test]
    fn state_rejects_tampered() {
        let key = b"a-test-key-32-bytes-or-more-aaaaaaaaaaaa";
        let org = OrgId::new();
        let user = UserId::new();
        let exp = 4_102_444_800;
        let mut token = sign_state(key, org, user, exp);
        // Flip the last hex char.
        let last = token.pop().expect("non-empty");
        token.push(if last == 'a' { 'b' } else { 'a' });
        assert!(verify_state(key, &token, 1_700_000_000).is_none());
    }

    #[test]
    fn state_rejects_wrong_key() {
        let k1: &[u8] = b"key-one-aaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let k2: &[u8] = b"key-two-bbbbbbbbbbbbbbbbbbbbbbbbbbb";
        let token = sign_state(k1, OrgId::new(), UserId::new(), 4_102_444_800);
        assert!(verify_state(k2, &token, 1_700_000_000).is_none());
    }
}
