//! HTTP routes for the Slack per-user identity link (issue #41, Alt A).
//!
//! An unlinked Slack user runs `/patom`; the slash response carries a
//! "Set up Patom" button to [`start`]. From there:
//!
//! 1. `GET /slack/identity/start` (public, root) — verifies the signed
//!    link token and bounces into the normal Patom login, carrying the
//!    token forward as a relative `return_to`. Auth is the token, not a
//!    cookie, so this lives outside the session gate.
//! 2. `GET /api/slack/identity/complete` (onboarding tier) — runs after
//!    login with a [`UserSession`] (which accepts an *org-less* session,
//!    so a brand-new user passes). Re-verifies the token, then binds the
//!    Slack user to the authenticated Patom account in the workspace's
//!    org. Auth-method-agnostic: it reads only the established session,
//!    never anything Google-specific, so a future email-magic-link login
//!    extends the flow with no change here.
//! 3. `DELETE /api/slack/identity/{team_id}/{slack_user_id}` (private
//!    tier) — unlink.

use axum::Router;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{delete, get};
use serde::Deserialize;
use tracing::{error, info, warn};

use crate::auth::{Principal, UserSession};
use crate::http::{AppState, HttpError};

use super::error::SlackError;
use super::identity::LinkedVia;
use super::link_token::verify_link;
use super::types::{SlackTeamId, SlackUserId};

#[derive(Debug, Deserialize)]
struct TokenQuery {
    #[serde(default)]
    token: Option<String>,
}

/// Public (no cookie): the "Set up Patom" button target.
pub fn start_router() -> Router<AppState> {
    Router::new().route("/slack/identity/start", get(start))
}

/// Onboarding tier (accepts an org-less session): post-login completion.
pub fn complete_router() -> Router<AppState> {
    Router::new().route("/slack/identity/complete", get(complete))
}

/// Private tier (established member): unlink.
pub fn unlink_router() -> Router<AppState> {
    Router::new().route("/slack/identity/{team_id}/{slack_user_id}", delete(unlink))
}

// ────────────────────────────────────────────────────────────────────
// GET /slack/identity/start
// ────────────────────────────────────────────────────────────────────

#[tracing::instrument(name = "slack.identity.start", skip_all)]
async fn start(State(state): State<AppState>, Query(q): Query<TokenQuery>) -> Response {
    match start_inner(&state, q.token.as_deref()) {
        Ok(login_url) => Redirect::to(&login_url).into_response(),
        Err(reason) => error_html(reason),
    }
}

fn start_inner(state: &AppState, token: Option<&str>) -> Result<String, &'static str> {
    let slack = state
        .slack
        .as_ref()
        .ok_or("Slack integration is not enabled on this Patom deployment.")?;
    let token = token.ok_or("Missing token.")?;
    let now = slack.clock.now_unix_secs();
    // Verify up front so an expired button shows a friendly page rather
    // than bouncing the user through a pointless login round trip.
    verify_link(slack.signing_secret.expose().as_bytes(), token, now).ok_or_else(|| {
        warn!(event = "slack.identity.bad_token");
        "This link has expired. Run `/patom` in Slack again to get a fresh one."
    })?;
    // Carry the SAME token forward; the completion route re-verifies it
    // after login.
    Ok(login_redirect_for(token))
}

/// Build the login redirect that carries the link token through to the
/// completion route. `return_to` is a relative path (so the login
/// handler's `sanitize_return_to` accepts it) and is form-encoded so the
/// token's `:` survives the round trip intact.
fn login_redirect_for(token: &str) -> String {
    let return_to = format!("/api/slack/identity/complete?token={token}");
    let query = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("return_to", &return_to)
        .finish();
    format!("/auth/oidc/login?{query}")
}

// ────────────────────────────────────────────────────────────────────
// GET /api/slack/identity/complete
// ────────────────────────────────────────────────────────────────────

#[tracing::instrument(name = "slack.identity.complete", skip_all)]
async fn complete(
    State(state): State<AppState>,
    session: UserSession,
    Query(q): Query<TokenQuery>,
) -> Response {
    match complete_inner(&state, &session, q.token.as_deref()).await {
        Ok(()) => success_html(),
        Err(reason) => error_html(reason),
    }
}

async fn complete_inner(
    state: &AppState,
    session: &UserSession,
    token: Option<&str>,
) -> Result<(), &'static str> {
    let slack = state
        .slack
        .as_ref()
        .ok_or("Slack integration is not enabled on this Patom deployment.")?;
    let token = token.ok_or("Missing token.")?;
    let now = slack.clock.now_unix_secs();
    let claims =
        verify_link(slack.signing_secret.expose().as_bytes(), token, now).ok_or_else(|| {
            warn!(event = "slack.identity.complete.bad_token");
            "This link has expired. Run `/patom` in Slack again to get a fresh one."
        })?;
    // The org is the workspace's, never the session's active org — a
    // freshly-onboarded user is org-less or in a personal org.
    let workspace = slack
        .workspaces
        .read_by_team(&claims.team_id)
        .await
        .map_err(|e| {
            warn!(error = ?e, event = "slack.identity.complete.workspace_missing");
            "Couldn't find the Slack workspace for this link."
        })?;
    slack
        .identities
        .link_with_org(
            session.user_id,
            workspace.org_id,
            &claims.team_id,
            &claims.slack_user_id,
            LinkedVia::SlackOauth,
        )
        .await
        .map_err(|e| {
            error!(error = ?e, event = "slack.identity.complete.link_failed");
            "Couldn't link your account. Please run `/patom` and try again."
        })?;
    info!(
        patom.org.id = %workspace.org_id.as_uuid(),
        patom.user.id = %session.user_id.as_uuid(),
        event = "slack.identity.linked",
    );
    // Capture the Slack display name (per-platform label) so the agent
    // refers to this person by their Slack handle in Slack threads — it is
    // stored on `slack_identities`, never on the canonical Patom user.
    // Best-effort; a `users.info` miss leaves it NULL (renderer falls back).
    if let Some(name) = crate::slack::bridge::fetch_slack_display_name(
        &slack.http,
        &workspace.bot_token,
        claims.slack_user_id.as_str(),
    )
    .await
        && let Err(e) = slack
            .identities
            .set_display_name(&claims.team_id, &claims.slack_user_id, &name)
            .await
    {
        warn!(error = ?e, event = "slack.identity.display_name_store_failed");
    }
    // Best-effort: swap the original "Set up Patom" ephemeral for a
    // success note so Slack reflects the link. Never fails the completion.
    if !claims.response_url.is_empty() {
        replace_slack_prompt(&slack.http, &claims.response_url).await;
    }
    Ok(())
}

/// `POST` the slash `response_url` with `replace_original: true` to turn
/// the "Set up Patom" button into a success message. Best-effort and
/// bounded; a failure (expired 30-min response_url, transport error) is
/// logged and swallowed — the browser already shows the success page.
async fn replace_slack_prompt(http: &reqwest::Client, response_url: &str) {
    let body = serde_json::json!({
        "response_type": "ephemeral",
        "replace_original": true,
        "text": "✅ Connected — run `/patom` in any channel to pick an agent.",
    });
    let send = http.post(response_url).json(&body).send();
    match tokio::time::timeout(std::time::Duration::from_secs(5), send).await {
        Ok(Ok(_)) => info!(event = "slack.identity.prompt_replaced"),
        Ok(Err(e)) => warn!(error = ?e, event = "slack.identity.prompt_replace_failed"),
        Err(_) => warn!(event = "slack.identity.prompt_replace_timeout"),
    }
}

// ────────────────────────────────────────────────────────────────────
// DELETE /api/slack/identity/{team_id}/{slack_user_id}
// ────────────────────────────────────────────────────────────────────

async fn unlink(
    State(state): State<AppState>,
    principal: Principal,
    Path((team_id, slack_user_id)): Path<(String, String)>,
) -> Result<StatusCode, HttpError> {
    let slack = state.slack.as_ref().ok_or(HttpError::NotFound)?;
    let team = SlackTeamId::try_from(team_id.as_str()).map_err(HttpError::Parse)?;
    let slack_user = SlackUserId::try_from(slack_user_id.as_str()).map_err(HttpError::Parse)?;
    slack
        .identities
        .unlink(&principal, &team, &slack_user)
        .await
        .map_err(unlink_to_http)?;
    Ok(StatusCode::NO_CONTENT)
}

fn unlink_to_http(e: SlackError) -> HttpError {
    match e {
        SlackError::Parse(p) => HttpError::Parse(p),
        other => {
            error!(error = ?other, event = "slack.identity.unlink_error");
            HttpError::Internal
        }
    }
}

// ────────────────────────────────────────────────────────────────────
// Browser-facing HTML (the only feedback channel for the GET routes)
// ────────────────────────────────────────────────────────────────────

fn page(title: &str, heading: &str, body: &str) -> String {
    format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><title>{title}</title></head>\
         <body style=\"font-family: -apple-system, system-ui, sans-serif; \
                       max-width: 32rem; margin: 4rem auto; line-height: 1.5; padding: 0 1rem;\">\
         <h1>{heading}</h1>{body}</body></html>",
    )
}

fn success_html() -> Response {
    let body = "<p>Your Slack account is now connected to Patom.</p>\
                <p>Head back to Slack and run <code>/patom</code> to pick an agent.</p>";
    Html(page("Connected to Patom", "You're all set", body)).into_response()
}

fn error_html(reason: &str) -> Response {
    let body = format!("<p>{}</p>", escape(reason));
    let mut resp = Html(page(
        "Couldn't connect",
        "Couldn't connect your account",
        &body,
    ))
    .into_response();
    *resp.status_mut() = StatusCode::BAD_REQUEST;
    resp
}

/// Minimal HTML escape — `reason` strings are short internal labels, so
/// the five mandatory characters are enough.
fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            other => out.push(other),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The login redirect must carry the token through `return_to` such
    /// that, once form-decoded, it is exactly the relative completion path
    /// the login handler will redirect back to — and that path must pass
    /// the `sanitize_return_to` relative-path rule (starts with `/`, not
    /// `//`). A signed link token contains `:`; this guards that it
    /// survives the round trip intact.
    #[test]
    fn login_redirect_round_trips_token_as_relative_return_to() {
        let token = "T0ABCDE:U0USER1:4102444800:deadbeef";
        let url = login_redirect_for(token);
        let (path, query) = url.split_once('?').expect("login url has a query");
        assert_eq!(path, "/auth/oidc/login");

        let return_to = url::form_urlencoded::parse(query.as_bytes())
            .find(|(k, _)| k == "return_to")
            .map(|(_, v)| v.into_owned())
            .expect("return_to present");
        assert_eq!(
            return_to,
            format!("/api/slack/identity/complete?token={token}")
        );
        // sanitize_return_to contract: relative path, not protocol-relative.
        assert!(return_to.starts_with('/'));
        assert!(!return_to.starts_with("//"));
    }
}
