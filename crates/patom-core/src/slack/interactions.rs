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
use super::types::{SlackBotToken, SlackChannelId, SlackTeamId, SlackUserId};

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
    let team_id = match SlackTeamId::try_from(parsed.team_id.as_str()) {
        Ok(t) => t,
        Err(e) => {
            warn!(error = %e, event = "slack.commands.bad_team_id");
            return StatusCode::BAD_REQUEST.into_response();
        }
    };
    tracing::Span::current().record("patom.slack.team", tracing::field::display(&team_id));
    let channel_id = match SlackChannelId::try_from(parsed.channel_id.as_str()) {
        Ok(c) => c,
        Err(e) => {
            warn!(error = %e, event = "slack.commands.bad_channel_id");
            return StatusCode::BAD_REQUEST.into_response();
        }
    };
    let user_id = match SlackUserId::try_from(parsed.user_id.as_str()) {
        Ok(u) => u,
        Err(e) => {
            warn!(error = %e, event = "slack.commands.bad_user_id");
            return StatusCode::BAD_REQUEST.into_response();
        }
    };

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

    let agents = match state.agents.list_for_org(workspace.org_id).await {
        Ok(rows) => rows,
        Err(e) => {
            warn!(error = ?e, event = "slack.commands.list_agents_failed");
            return ephemeral_message("Couldn't load the agent roster. Try again in a moment.");
        }
    };

    let metadata = build_private_metadata(&team_id, &channel_id, &user_id, &parsed.user_name);
    if metadata.len() > MAX_PRIVATE_METADATA_BYTES {
        // Impossible in practice (three short ids), but assert at the
        // boundary so a future field addition cannot silently overflow.
        warn!(
            len = metadata.len(),
            event = "slack.commands.private_metadata_overflow"
        );
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    let modal = build_compose_modal(&agents, &metadata);
    // Modals cannot ride back in the slash command response body —
    // Slack only opens them via `views.open` with a fresh `trigger_id`
    // (valid for 3 seconds from the original invocation).
    if let Err(e) = open_view(
        &slack.http,
        &workspace.bot_token,
        &parsed.trigger_id,
        &modal,
    )
    .await
    {
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
        sessions: state.sessions.clone(),
        colleagues: state.colleagues.clone(),
        workspaces: slack.workspaces.clone(),
        identities: slack.identities.clone(),
        threads: slack.threads.clone(),
        poster: slack.poster.clone(),
        stream_pump: slack.stream_pump.clone(),
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
    Ok(SlashCommandSubmit {
        team_id,
        channel_id,
        slack_user_id,
        slack_user_name: routing.user_name,
        agent_id,
        prompt,
        view_id: view.view.id.clone(),
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

fn build_private_metadata(
    team_id: &SlackTeamId,
    channel_id: &SlackChannelId,
    user_id: &SlackUserId,
    user_name: &str,
) -> String {
    json!({
        "team_id": team_id.as_str(),
        "channel_id": channel_id.as_str(),
        "user_id": user_id.as_str(),
        "user_name": user_name,
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
        for (k, v) in url::form_urlencoded::parse(body.as_ref()) {
            match k.as_ref() {
                "command" => command = Some(v.into_owned()),
                "team_id" => team_id = Some(v.into_owned()),
                "channel_id" => channel_id = Some(v.into_owned()),
                "user_id" => user_id = Some(v.into_owned()),
                "user_name" => user_name = Some(v.into_owned()),
                "trigger_id" => trigger_id = Some(v.into_owned()),
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
    #[serde(other)]
    Other,
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
        let s = build_private_metadata(&team, &chan, &user, "tomkapa");
        let parsed: PrivateMetadata = serde_json::from_str(&s).expect("decode");
        assert_eq!(parsed.team_id, "T123");
        assert_eq!(parsed.channel_id, "C456");
        assert_eq!(parsed.user_id, "U789");
        assert_eq!(parsed.user_name, "tomkapa");
        assert!(s.len() <= MAX_PRIVATE_METADATA_BYTES);
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
}
