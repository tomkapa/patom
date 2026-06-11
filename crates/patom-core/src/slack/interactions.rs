//! Slack Interactivity & Slash Command webhook handlers.
//!
//! Two public routes:
//!
//! 1. `POST /slack/commands` — Slack POSTs every `/patom` invocation
//!    here. Form-encoded body, no `payload=` wrapper. Handler verifies
//!    the HMAC, looks up the tenant by `team_id`, fetches the agent
//!    roster, and opens a Block Kit modal via `views.open` using the
//!    short-lived `trigger_id` Slack supplies. Slash command responses
//!    cannot return a modal inline — that surface only accepts text /
//!    blocks — so the modal arrives through the separate Web API call.
//!    The handler must call `views.open` AND return 200 within Slack's
//!    3 s ack window; both steps are bounded and synchronous.
//!
//! 2. `POST /slack/interactions` — Slack POSTs `view_submission`
//!    (and other interactivity) here. Form-encoded body with
//!    `payload=<URL-encoded JSON>`. The handler parses the envelope,
//!    extracts the chosen agent + prompt + routing metadata, hands
//!    them to [`super::bridge::enqueue_from_slash`], and returns
//!    `{"response_action": "clear"}` to close the modal.
//!
//! Both routes reuse [`super::verify::verify`] for signature checking;
//! the verifier operates over the raw body bytes, so we use the `Bytes`
//! extractor (not `Form<T>`) to keep MAC alignment with what Slack
//! signed.

use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};
use tracing::{info, warn};

use crate::agents::AgentId;
use crate::http::AppState;
use crate::types::Prompt;

use super::bridge::{SlashCommandSubmit, enqueue_from_slash};
use super::events::check_signature;
use super::limits::{
    MAX_PRIVATE_METADATA_BYTES, SLACK_POST_BODY_TIMEOUT, SLACK_POST_TIMEOUT,
    SLACK_WEBHOOK_MAX_BYTES,
};
use super::modal::{
    AGENT_ACTION_ID, AGENT_BLOCK_ID, COMPOSE_CALLBACK_ID, PROMPT_ACTION_ID, PROMPT_BLOCK_ID,
    build_compose_modal,
};
use super::types::{SlackBotToken, SlackChannelId, SlackTeamId, SlackThreadTs, SlackUserId};

/// Command literal Slack sends in `command` for the `/patom`
/// invocation. Must match the value registered in the Slack app
/// manifest; a mismatch surfaces as 404 so a misconfigured DNS or
/// shared URL does not 500 on us.
const SLASH_COMMAND: &str = "/patom";

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/slack/commands", post(handle_slash))
        .route("/slack/interactions", post(handle_interaction))
        .layer(DefaultBodyLimit::max(SLACK_WEBHOOK_MAX_BYTES))
}

// ────────────────────────────────────────────────────────────────────
// Slash command (`POST /slack/commands`)
// ────────────────────────────────────────────────────────────────────

#[tracing::instrument(
    name = "slack.commands.handle",
    skip_all,
    fields(
        patom.slack.team = tracing::field::Empty,
        patom.tenant.id = tracing::field::Empty,
    ),
)]
async fn handle_slash(state: State<AppState>, headers: HeaderMap, body: Bytes) -> Response {
    let Some(slack) = state.slack.as_ref() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if let Err(status) = check_signature(slack, &headers, &body) {
        return status.into_response();
    }
    let parsed = match SlashPayload::from_form(&body) {
        Ok(p) => p,
        Err(status) => return status.into_response(),
    };
    if parsed.command != SLASH_COMMAND {
        warn!(
            command = %parsed.command,
            event = "slack.commands.unknown_command",
        );
        return StatusCode::NOT_FOUND.into_response();
    }
    let (team_id, channel_id, user_id) = match parse_slash_ids(&parsed) {
        Ok(ids) => ids,
        Err(status) => return status.into_response(),
    };
    tracing::Span::current().record("patom.slack.team", tracing::field::display(&team_id));

    let workspace = match slack.workspaces.read_by_team(&team_id).await {
        Ok(w) => w,
        Err(e) => {
            warn!(error = ?e, event = "slack.commands.unknown_workspace");
            return ephemeral_message(
                "This Slack workspace is not connected to Patom. \
                 Ask an admin to install Patom first.",
            );
        }
    };
    tracing::Span::current().record(
        "patom.tenant.id",
        tracing::field::display(workspace.org_id.as_uuid()),
    );

    // Gate the composer on a linked identity: `/patom` acts *as* the
    // invoking user, so we need to know who they are (issue #41). An
    // unlinked user gets a "Set up Patom" button instead of the picker.
    match slack.identities.lookup(&team_id, &user_id).await {
        Ok(Some(_)) => {}
        Ok(None) => {
            info!(event = "slack.commands.unlinked_prompted");
            return link_prompt(slack, &team_id, &user_id, &parsed.response_url);
        }
        Err(e) => {
            warn!(error = ?e, event = "slack.commands.identity_lookup_failed");
            return ephemeral_message("Couldn't check your Patom account. Try again in a moment.");
        }
    }

    // Slash `/patom` carries no thread context (Slack omits `thread_ts`
    // from the slash form), so a new top-level thread is started.
    open_compose_modal(
        &state,
        slack,
        &workspace,
        &parsed.trigger_id,
        &team_id,
        &channel_id,
        &user_id,
        &parsed.user_name,
        None,
    )
    .await
}

/// Resolve the agent roster and open the compose modal via `views.open`,
/// stashing routing context (incl. optional `thread_ts`) in
/// `private_metadata`. Shared by `/patom` and the "Ask Patom" message
/// shortcut. Returns the `200`/error response the interactivity request
/// should ack with.
#[allow(clippy::too_many_arguments)]
async fn open_compose_modal(
    state: &AppState,
    slack: &super::SlackAppState,
    workspace: &super::workspace::WorkspaceWithToken,
    trigger_id: &str,
    team_id: &SlackTeamId,
    channel_id: &SlackChannelId,
    user_id: &SlackUserId,
    user_name: &str,
    thread_ts: Option<&SlackThreadTs>,
) -> Response {
    let agents = match state.agents.list_for_org(workspace.org_id).await {
        Ok(rows) => rows,
        Err(e) => {
            warn!(error = ?e, event = "slack.commands.list_agents_failed");
            return ephemeral_message("Couldn't load the agent roster. Try again in a moment.");
        }
    };
    let metadata = build_private_metadata(team_id, channel_id, user_id, user_name, thread_ts);
    if metadata.len() > MAX_PRIVATE_METADATA_BYTES {
        // Impossible in practice (short ids + one ts), but assert at the
        // boundary so a future field addition cannot silently overflow.
        warn!(
            len = metadata.len(),
            event = "slack.commands.private_metadata_overflow"
        );
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    let modal = build_compose_modal(&agents, &metadata);
    // Modals open only via `views.open` with a fresh `trigger_id` (valid
    // ~3 s) — they cannot ride back in the interactivity response body.
    if let Err(e) = open_view(&slack.http, &workspace.bot_token, trigger_id, &modal).await {
        warn!(error = ?e, event = "slack.commands.views_open_failed");
        return ephemeral_message(
            "Couldn't open the Patom composer. Check your Slack app permissions, then try again.",
        );
    }
    info!(
        patom.tenant.id = %workspace.org_id.as_uuid(),
        patom.slack.team = %team_id,
        agents = agents.len(),
        event = "slack.commands.modal_opened",
    );
    // Slack expects a 200 within 3 s; empty body is the documented
    // "modal opened separately" pattern.
    StatusCode::OK.into_response()
}

/// `views.open` Web API call. Returns `Err` on HTTP error, transport
/// failure, or a `{ok:false}` body. The caller surfaces an ephemeral
/// message to the user on failure; the modal never opens in that case.
async fn open_view(
    http: &reqwest::Client,
    token: &SlackBotToken,
    trigger_id: &str,
    view: &Value,
) -> Result<(), super::error::SlackError> {
    #[derive(serde::Deserialize)]
    struct OpenResp {
        ok: bool,
        #[serde(default)]
        error: Option<String>,
    }
    let body = json!({ "trigger_id": trigger_id, "view": view });
    let send = http
        .post("https://slack.com/api/views.open")
        .bearer_auth(token.expose())
        .json(&body)
        .send();
    let resp = match tokio::time::timeout(SLACK_POST_TIMEOUT, send).await {
        Ok(Ok(r)) => r,
        Ok(Err(e)) => return Err(e.into()),
        Err(_) => return Err(super::error::SlackError::PostTimeout(SLACK_POST_TIMEOUT)),
    };
    let status = resp.status();
    if !status.is_success() {
        // Bound the diagnostic body read so a hung Slack edge cannot
        // stall the slash command handler past the 3 s ack window
        // (CLAUDE.md §5: every await against I/O is timed).
        let body = match tokio::time::timeout(SLACK_POST_BODY_TIMEOUT, resp.text()).await {
            Ok(Ok(s)) => s,
            Ok(Err(_)) | Err(_) => String::new(),
        };
        return Err(super::error::SlackError::PostFailed {
            status: status.as_u16(),
            body,
        });
    }
    let parsed: OpenResp = match tokio::time::timeout(SLACK_POST_BODY_TIMEOUT, resp.json()).await {
        Ok(Ok(p)) => p,
        Ok(Err(e)) => return Err(super::error::SlackError::Http(e)),
        Err(_) => {
            return Err(super::error::SlackError::PostTimeout(
                SLACK_POST_BODY_TIMEOUT,
            ));
        }
    };
    if !parsed.ok {
        return Err(super::error::SlackError::PostFailed {
            status: 200,
            body: parsed.error.unwrap_or_else(|| "unknown".to_owned()),
        });
    }
    Ok(())
}

// ────────────────────────────────────────────────────────────────────
// View submission (`POST /slack/interactions`)
// ────────────────────────────────────────────────────────────────────

#[tracing::instrument(
    name = "slack.interactions.handle",
    skip_all,
    fields(payload_type = tracing::field::Empty),
)]
async fn handle_interaction(state: State<AppState>, headers: HeaderMap, body: Bytes) -> Response {
    let Some(slack) = state.slack.as_ref() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if let Err(status) = check_signature(slack, &headers, &body) {
        return status.into_response();
    }
    let payload_json = match extract_payload(&body) {
        Ok(v) => v,
        Err(status) => return status.into_response(),
    };
    let envelope: InteractionEnvelope = match serde_json::from_value(payload_json) {
        Ok(e) => e,
        Err(e) => {
            warn!(error = %e, event = "slack.interactions.envelope_decode_failed");
            return StatusCode::BAD_REQUEST.into_response();
        }
    };
    match envelope {
        InteractionEnvelope::ViewSubmission(view) => {
            tracing::Span::current().record("payload_type", "view_submission");
            handle_view_submission(state, view).await
        }
        InteractionEnvelope::MessageAction(action) => {
            tracing::Span::current().record("payload_type", "message_action");
            handle_message_action(state, action).await
        }
        InteractionEnvelope::Other => {
            tracing::Span::current().record("payload_type", "other");
            // Ack any forward-compatible interactivity type we don't
            // route on yet so Slack does not retry.
            StatusCode::OK.into_response()
        }
    }
}

async fn handle_view_submission(state: State<AppState>, view: ViewSubmission) -> Response {
    if view.view.callback_id != COMPOSE_CALLBACK_ID {
        // Different modal flow not implemented yet; ack and move on.
        return StatusCode::OK.into_response();
    }
    let Some(slack) = state.slack.as_ref() else {
        return StatusCode::NOT_FOUND.into_response();
    };

    let submit = match build_submit(&view) {
        Ok(s) => s,
        Err(errors) => return validation_errors_response(errors),
    };

    let deps = super::bridge::BridgeDeps {
        queue: state.queue.clone(),
        agents: state.agents.clone(),
        // No `AppState.threads` field — build the thread store inline from the
        // pool/clock seam (note 18), matching the HTTP thread routes.
        thread_store: std::sync::Arc::new(crate::threads::PgThreadStore::new(
            state.pool.clone(),
            state.clock.clone(),
        )),
        colleagues: state.colleagues.clone(),
        workspaces: slack.workspaces.clone(),
        identities: slack.identities.clone(),
        // Built inline from the pool/clock seam, like `thread_store` above.
        channels_map: std::sync::Arc::new(crate::slack::channel_map::PgSlackChannelStore::new(
            state.pool.clone(),
            state.clock.clone(),
        )),
        threads: slack.threads.clone(),
        poster: slack.poster.clone(),
        stream_pump: slack.stream_pump.clone(),
        pool: state.pool.clone(),
        http: slack.http.clone(),
    };
    if let Err(e) = enqueue_from_slash(&deps, submit).await {
        warn!(error = ?e, event = "slack.interactions.enqueue_failed");
        // Surface a generic error in the modal — the operator sees the
        // detail in logs; the user gets a non-actionable retry hint.
        return validation_errors_response(vec![(
            PROMPT_BLOCK_ID,
            "Couldn't send to Patom. Please try again.".to_owned(),
        )]);
    }
    info!(event = "slack.interactions.view_submitted");
    (StatusCode::OK, Json(json!({ "response_action": "clear" }))).into_response()
}

/// Handle the "Ask Patom" message shortcut: open the compose modal from
/// inside a Slack thread, carrying the message's `thread_ts` so the
/// submission posts into that thread (issue #41, point 2). Gated on a
/// linked identity, like `/patom`. Inert unless the operator added the
/// shortcut to the app manifest with [`ASK_PATOM_SHORTCUT_ID`].
async fn handle_message_action(state: State<AppState>, action: MessageAction) -> Response {
    if action.callback_id != super::modal::ASK_PATOM_SHORTCUT_ID {
        return StatusCode::OK.into_response();
    }
    let Some(slack) = state.slack.as_ref() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let (team_id, channel_id, user_id) =
        match parse_ids(&action.team.id, &action.channel.id, &action.user.id) {
            Ok(ids) => ids,
            Err(status) => return status.into_response(),
        };
    // Anchor on the message's thread (or the message itself if top-level).
    let anchor_raw = action
        .message
        .thread_ts
        .as_deref()
        .unwrap_or(action.message.ts.as_str());
    let thread_ts = SlackThreadTs::try_from(anchor_raw).ok();

    let workspace = match slack.workspaces.read_by_team(&team_id).await {
        Ok(w) => w,
        Err(e) => {
            warn!(error = ?e, event = "slack.shortcut.unknown_workspace");
            return StatusCode::OK.into_response();
        }
    };
    match slack.identities.lookup(&team_id, &user_id).await {
        Ok(Some(_)) => {}
        Ok(None) => {
            // No response_url is rendered for a message_action body, so
            // nudge via an ephemeral post instead.
            nudge_link_via_ephemeral(slack, &workspace, &channel_id, &user_id).await;
            return StatusCode::OK.into_response();
        }
        Err(e) => {
            warn!(error = ?e, event = "slack.shortcut.identity_lookup_failed");
            return StatusCode::OK.into_response();
        }
    }
    open_compose_modal(
        &state,
        slack,
        &workspace,
        &action.trigger_id,
        &team_id,
        &channel_id,
        &user_id,
        &action.user.username,
        thread_ts.as_ref(),
    )
    .await
}

/// Best-effort ephemeral "connect your account" nudge for the shortcut
/// path (where a slash-style ephemeral response body is not rendered).
async fn nudge_link_via_ephemeral(
    slack: &super::SlackAppState,
    workspace: &super::workspace::WorkspaceWithToken,
    channel_id: &SlackChannelId,
    user_id: &SlackUserId,
) {
    let req = super::poster::PostRequest {
        token: workspace.bot_token.clone(),
        channel: channel_id.clone(),
        thread_ts: None,
        body: super::poster::PostBody::Text(
            "Connect your Patom account first — run `/patom` in this channel.".to_owned(),
        ),
        username: "Patom".to_owned(),
        icon_url: None,
        ephemeral_to: Some(user_id.clone()),
    };
    if let Err(e) = slack.poster.post(req).await {
        warn!(error = ?e, event = "slack.shortcut.nudge_failed");
    }
}

/// Parse the chosen agent + prompt + routing metadata out of the
/// `view.state.values` map. Returns a list of `(block_id, message)`
/// errors when fields are missing or malformed; Slack re-opens the
/// modal with red badges on those blocks.
fn build_submit(view: &ViewSubmission) -> Result<SlashCommandSubmit, Vec<(&'static str, String)>> {
    let mut errors: Vec<(&'static str, String)> = Vec::new();

    let agent_id = view
        .view
        .state
        .values
        .get(AGENT_BLOCK_ID)
        .and_then(|block| block.get(AGENT_ACTION_ID))
        .and_then(|el| el.selected_option.as_ref())
        .and_then(|opt| uuid::Uuid::parse_str(&opt.value).ok())
        .map(AgentId::from);
    if agent_id.is_none() {
        errors.push((AGENT_BLOCK_ID, "Pick an agent.".to_owned()));
    }

    let prompt = view
        .view
        .state
        .values
        .get(PROMPT_BLOCK_ID)
        .and_then(|block| block.get(PROMPT_ACTION_ID))
        .and_then(|el| el.value.as_deref())
        .and_then(|s| Prompt::try_from(s).ok());
    if prompt.is_none() {
        errors.push((PROMPT_BLOCK_ID, "Write a prompt to send.".to_owned()));
    }

    let Ok(routing) = serde_json::from_str::<PrivateMetadata>(&view.view.private_metadata) else {
        warn!(event = "slack.interactions.private_metadata_decode_failed");
        return Err(vec![(
            PROMPT_BLOCK_ID,
            "Internal routing payload was malformed. Re-run /patom.".to_owned(),
        )]);
    };
    let team_id = SlackTeamId::try_from(routing.team_id.as_str())
        .map_err(|e| vec![(PROMPT_BLOCK_ID, format!("Bad team id: {e}"))])?;
    let channel_id = SlackChannelId::try_from(routing.channel_id.as_str())
        .map_err(|e| vec![(PROMPT_BLOCK_ID, format!("Bad channel id: {e}"))])?;
    let slack_user_id = SlackUserId::try_from(routing.user_id.as_str())
        .map_err(|e| vec![(PROMPT_BLOCK_ID, format!("Bad user id: {e}"))])?;

    if !errors.is_empty() {
        return Err(errors);
    }

    let agent_id = agent_id.expect("invariant: errors empty implies agent_id present");
    let prompt = prompt.expect("invariant: errors empty implies prompt present");
    let thread_ts = routing
        .thread_ts
        .as_deref()
        .and_then(|s| SlackThreadTs::try_from(s).ok());
    Ok(SlashCommandSubmit {
        team_id,
        channel_id,
        slack_user_id,
        slack_user_name: routing.user_name,
        agent_id,
        prompt,
        view_id: view.view.id.clone(),
        thread_ts,
    })
}

fn validation_errors_response(errors: Vec<(&'static str, String)>) -> Response {
    let errors_json: serde_json::Map<String, Value> = errors
        .into_iter()
        .map(|(block, msg)| ((*block).to_owned(), Value::String(msg)))
        .collect();
    (
        StatusCode::OK,
        Json(json!({
            "response_action": "errors",
            "errors": errors_json,
        })),
    )
        .into_response()
}

// ────────────────────────────────────────────────────────────────────
// Shared helpers
// ────────────────────────────────────────────────────────────────────

fn extract_payload(body: &Bytes) -> Result<Value, StatusCode> {
    // Stream the urlencoded pairs once — no `.into_owned().collect()`
    // intermediate allocation. We only own the one value we need.
    let payload = url::form_urlencoded::parse(body.as_ref())
        .find_map(|(k, v)| (k == "payload").then(|| v.into_owned()));
    let Some(payload) = payload else {
        warn!(event = "slack.interactions.missing_payload_field");
        return Err(StatusCode::BAD_REQUEST);
    };
    serde_json::from_str(&payload).map_err(|e| {
        warn!(error = %e, event = "slack.interactions.payload_decode_failed");
        StatusCode::BAD_REQUEST
    })
}

fn ephemeral_message(text: &str) -> Response {
    (
        StatusCode::OK,
        Json(json!({ "response_type": "ephemeral", "text": text })),
    )
        .into_response()
}

/// Parse the three Slack ids off a slash payload, returning a
/// `400`-shaped response on any malformed value. Extracted so
/// `handle_slash` stays within the function-length ceiling (§4).
fn parse_slash_ids(
    parsed: &SlashPayload,
) -> Result<(SlackTeamId, SlackChannelId, SlackUserId), StatusCode> {
    parse_ids(&parsed.team_id, &parsed.channel_id, &parsed.user_id)
}

/// Parse the three Slack ids from raw strings, mapping any malformed value
/// to `400`. Shared by the slash and message-shortcut entry points.
fn parse_ids(
    team: &str,
    channel: &str,
    user: &str,
) -> Result<(SlackTeamId, SlackChannelId, SlackUserId), StatusCode> {
    let team_id = SlackTeamId::try_from(team).map_err(|e| {
        warn!(error = %e, event = "slack.commands.bad_team_id");
        StatusCode::BAD_REQUEST
    })?;
    let channel_id = SlackChannelId::try_from(channel).map_err(|e| {
        warn!(error = %e, event = "slack.commands.bad_channel_id");
        StatusCode::BAD_REQUEST
    })?;
    let user_id = SlackUserId::try_from(user).map_err(|e| {
        warn!(error = %e, event = "slack.commands.bad_user_id");
        StatusCode::BAD_REQUEST
    })?;
    Ok((team_id, channel_id, user_id))
}

/// Ephemeral slash response inviting an unlinked user to connect their
/// Patom account. The button is a `url`-type Block Kit action pointing at
/// `GET /slack/identity/start` with a signed link token bound to this
/// `(team, user)` — Slack opens it in the browser, where the user logs in
/// and the completion route writes the link. Ephemeral, so it is visible
/// only to the invoking user.
fn link_prompt(
    slack: &super::SlackAppState,
    team_id: &SlackTeamId,
    user_id: &SlackUserId,
    response_url: &str,
) -> Response {
    let exp = slack.clock.now_unix_secs() + super::link_token::LINK_TOKEN_TTL_SECS;
    let token = super::link_token::sign_link(
        slack.signing_secret.expose().as_bytes(),
        &super::link_token::SlackLinkClaims {
            team_id: team_id.clone(),
            slack_user_id: user_id.clone(),
            response_url: response_url.to_owned(),
        },
        exp,
    );
    let url = format!(
        "{}/slack/identity/start?token={token}",
        slack.public_base_url
    );
    let blocks = json!([
        {
            "type": "section",
            "text": {
                "type": "mrkdwn",
                "text": "Connect your Patom account to chat with agents from Slack. \
                         This links your Slack identity so agents know it's you.",
            },
        },
        {
            "type": "actions",
            "elements": [{
                "type": "button",
                "style": "primary",
                "text": { "type": "plain_text", "text": "Set up Patom" },
                "url": url,
            }],
        },
    ]);
    (
        StatusCode::OK,
        Json(json!({
            "response_type": "ephemeral",
            "text": "Connect your Patom account to get started.",
            "blocks": blocks,
        })),
    )
        .into_response()
}

fn build_private_metadata(
    team_id: &SlackTeamId,
    channel_id: &SlackChannelId,
    user_id: &SlackUserId,
    user_name: &str,
    thread_ts: Option<&SlackThreadTs>,
) -> String {
    json!({
        "team_id": team_id.as_str(),
        "channel_id": channel_id.as_str(),
        "user_id": user_id.as_str(),
        "user_name": user_name,
        // Present only for the message-shortcut path, so view_submission
        // continues the existing thread rather than starting a new one.
        "thread_ts": thread_ts.map(SlackThreadTs::as_str),
    })
    .to_string()
}

// ────────────────────────────────────────────────────────────────────
// Wire types — minimal projections we read.
// ────────────────────────────────────────────────────────────────────

/// Slash command POST body (Slack sends form-encoded; we project the
/// fields we route on). `trigger_id` is required to call `views.open`
/// — without it, the modal cannot be displayed for this invocation.
#[derive(Debug, Clone)]
struct SlashPayload {
    command: String,
    team_id: String,
    channel_id: String,
    user_id: String,
    /// User's @handle (e.g. `tomkapa`). Slack always includes this in
    /// the slash command form; we cache it in `private_metadata` so
    /// it survives to the `view_submission` handler.
    user_name: String,
    trigger_id: String,
    /// Slack's per-invocation callback URL. Used to `replace_original` the
    /// "Set up Patom" ephemeral with a success message after the user
    /// links (issue #41). Empty if Slack omitted it — the link flow still
    /// works, it just can't update the prompt in place.
    response_url: String,
}

impl SlashPayload {
    fn from_form(body: &Bytes) -> Result<Self, StatusCode> {
        // Stream the pairs once; only own the values we actually keep.
        // Keys are borrowed `Cow<str>` from the decoder — match without
        // allocating.
        let mut command = None;
        let mut team_id = None;
        let mut channel_id = None;
        let mut user_id = None;
        let mut user_name = None;
        let mut trigger_id = None;
        let mut response_url = String::new();
        for (k, v) in url::form_urlencoded::parse(body.as_ref()) {
            match k.as_ref() {
                "command" => command = Some(v.into_owned()),
                "team_id" => team_id = Some(v.into_owned()),
                "channel_id" => channel_id = Some(v.into_owned()),
                "user_id" => user_id = Some(v.into_owned()),
                "user_name" => user_name = Some(v.into_owned()),
                "trigger_id" => trigger_id = Some(v.into_owned()),
                "response_url" => response_url = v.into_owned(),
                _ => {}
            }
        }
        let (
            Some(command),
            Some(team_id),
            Some(channel_id),
            Some(user_id),
            Some(user_name),
            Some(trigger_id),
        ) = (command, team_id, channel_id, user_id, user_name, trigger_id)
        else {
            warn!(event = "slack.commands.missing_form_fields");
            return Err(StatusCode::BAD_REQUEST);
        };
        Ok(Self {
            command,
            team_id,
            channel_id,
            user_id,
            user_name,
            trigger_id,
            response_url,
        })
    }
}

/// Top-level Slack interactivity envelope (the JSON inside `payload=`).
/// We only route `view_submission`; everything else acks and falls
/// through.
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum InteractionEnvelope {
    ViewSubmission(ViewSubmission),
    MessageAction(MessageAction),
    #[serde(other)]
    Other,
}

/// A message shortcut invocation ("Ask Patom" on a message). Carries the
/// originating message so the modal can continue that Slack thread.
#[derive(Debug, Deserialize)]
struct MessageAction {
    callback_id: String,
    trigger_id: String,
    team: IdField,
    channel: IdField,
    user: ShortcutUser,
    message: ShortcutMessage,
}

#[derive(Debug, Deserialize)]
struct IdField {
    id: String,
}

#[derive(Debug, Deserialize)]
struct ShortcutUser {
    id: String,
    #[serde(default)]
    username: String,
}

#[derive(Debug, Deserialize)]
struct ShortcutMessage {
    ts: String,
    /// Set when the shortcut is used on a threaded reply; absent on a
    /// top-level message (then `ts` itself anchors the thread).
    #[serde(default)]
    thread_ts: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ViewSubmission {
    view: View,
}

#[derive(Debug, Deserialize)]
struct View {
    id: String,
    callback_id: String,
    #[serde(default)]
    private_metadata: String,
    state: ViewState,
}

#[derive(Debug, Deserialize)]
struct ViewState {
    values: std::collections::HashMap<String, std::collections::HashMap<String, ViewElement>>,
}

#[derive(Debug, Deserialize)]
struct ViewElement {
    #[serde(default)]
    value: Option<String>,
    #[serde(default)]
    selected_option: Option<SelectedOption>,
}

#[derive(Debug, Deserialize)]
struct SelectedOption {
    value: String,
}

#[derive(Debug, Deserialize)]
#[allow(clippy::struct_field_names)]
struct PrivateMetadata {
    team_id: String,
    channel_id: String,
    user_id: String,
    /// Slack `user_name` (e.g. `tomkapa`). Used as the attribution
    /// label on the synthetic prompt-mirror post; required so the
    /// post reads as that user rather than the raw `U…` id.
    user_name: String,
    /// `Some` only when the modal was opened from a message shortcut in a
    /// thread — the message's `thread_ts`. Drives in-thread continuation.
    #[serde(default)]
    thread_ts: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slash_payload_extracts_required_fields() {
        let body = Bytes::from(
            "command=%2Fpatom&team_id=T123&channel_id=C456&user_id=U789&\
             user_name=tomkapa&text=hello&trigger_id=ABC.123",
        );
        let p = SlashPayload::from_form(&body).expect("parse");
        assert_eq!(p.command, "/patom");
        assert_eq!(p.team_id, "T123");
        assert_eq!(p.channel_id, "C456");
        assert_eq!(p.user_id, "U789");
        assert_eq!(p.user_name, "tomkapa");
        assert_eq!(p.trigger_id, "ABC.123");
    }

    #[test]
    fn slash_payload_rejects_missing_trigger_id() {
        // Without trigger_id we cannot open a modal — fail fast.
        let body = Bytes::from(
            "command=%2Fpatom&team_id=T123&channel_id=C456&user_id=U789&user_name=tomkapa",
        );
        assert!(SlashPayload::from_form(&body).is_err());
    }

    #[test]
    fn slash_payload_rejects_missing_fields() {
        let body = Bytes::from("command=%2Fpatom&team_id=T123");
        assert!(SlashPayload::from_form(&body).is_err());
    }

    #[test]
    fn private_metadata_round_trip() {
        let team = SlackTeamId::try_from("T123").expect("ok");
        let chan = SlackChannelId::try_from("C456").expect("ok");
        let user = SlackUserId::try_from("U789").expect("ok");
        // Slash path: no thread context.
        let s = build_private_metadata(&team, &chan, &user, "tomkapa", None);
        let parsed: PrivateMetadata = serde_json::from_str(&s).expect("decode");
        assert_eq!(parsed.team_id, "T123");
        assert_eq!(parsed.channel_id, "C456");
        assert_eq!(parsed.user_id, "U789");
        assert_eq!(parsed.user_name, "tomkapa");
        assert_eq!(parsed.thread_ts, None);
        assert!(s.len() <= MAX_PRIVATE_METADATA_BYTES);

        // Shortcut path: the message thread_ts round-trips so the
        // submission continues that thread.
        let ts = SlackThreadTs::try_from("1700000000.000200").expect("ok");
        let s2 = build_private_metadata(&team, &chan, &user, "tomkapa", Some(&ts));
        let parsed2: PrivateMetadata = serde_json::from_str(&s2).expect("decode");
        assert_eq!(parsed2.thread_ts.as_deref(), Some("1700000000.000200"));
    }

    #[test]
    fn extract_payload_decodes_url_encoded_json() {
        // Build a real form body the way Slack does it: payload=<URL-encoded-JSON>
        let raw_json = r#"{"type":"view_submission","view":{"id":"V1","callback_id":"x","private_metadata":"","state":{"values":{}}}}"#;
        let encoded: String = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("payload", raw_json)
            .finish();
        let body = Bytes::from(encoded);
        let v = extract_payload(&body).expect("decode");
        assert_eq!(v["type"], "view_submission");
        assert_eq!(v["view"]["id"], "V1");
    }

    #[test]
    fn message_action_envelope_parses_thread_context() {
        // A realistic Slack message_action payload (trimmed to the fields
        // we route on). `message.thread_ts` is what drives in-thread
        // continuation.
        let raw = r#"{
            "type": "message_action",
            "callback_id": "patom_ask_in_thread",
            "trigger_id": "T.123",
            "team": {"id": "T0TEAM"},
            "channel": {"id": "C0CHAN"},
            "user": {"id": "U0USER", "username": "tomkapa"},
            "message": {"ts": "1700000000.000100", "thread_ts": "1700000000.000050"}
        }"#;
        let env: InteractionEnvelope = serde_json::from_str(raw).expect("decode");
        let InteractionEnvelope::MessageAction(a) = env else {
            panic!("expected MessageAction");
        };
        assert_eq!(a.callback_id, "patom_ask_in_thread");
        assert_eq!(a.channel.id, "C0CHAN");
        assert_eq!(a.user.username, "tomkapa");
        assert_eq!(a.message.thread_ts.as_deref(), Some("1700000000.000050"));
    }

    #[test]
    fn message_action_without_thread_ts_anchors_on_message_ts() {
        // Shortcut used on a top-level message: no thread_ts, so the
        // handler anchors on `message.ts`.
        let raw = r#"{
            "type": "message_action",
            "callback_id": "patom_ask_in_thread",
            "trigger_id": "T.1",
            "team": {"id": "T0TEAM"},
            "channel": {"id": "C0CHAN"},
            "user": {"id": "U0USER"},
            "message": {"ts": "1700000000.000100"}
        }"#;
        let env: InteractionEnvelope = serde_json::from_str(raw).expect("decode");
        let InteractionEnvelope::MessageAction(a) = env else {
            panic!("expected MessageAction");
        };
        assert_eq!(a.message.thread_ts, None);
        assert_eq!(a.message.ts, "1700000000.000100");
        assert_eq!(a.user.username, "", "username defaults when absent");
    }
}
