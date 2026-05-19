//! Webhook handler — `POST /slack/events`.
//!
//! Three jobs, in order:
//!
//! 1. Verify the HMAC-SHA256 signature on the *raw* bytes. The
//!    extractor is `Bytes`, not `Json<T>`; any tower/axum layer that
//!    rewrites the body before this point will desync the MAC.
//! 2. Handle `url_verification` inline — Slack expects the
//!    `challenge` value echoed back on the install ping.
//! 3. Parse the envelope, lift the inbound shape into our typed
//!    [`InboundEvent`], and hand it to the bridge worker via
//!    `mpsc::try_send`. The handler returns `200 OK` either way; a
//!    full queue surfaces as a counter (Slack will retry).
//!
//! The handler MUST stay under [`SLACK_ACK_BUDGET`] — no DB calls, no
//! HTTP egress, no agent work. Everything slow lives in `bridge.rs`.

use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};
use tracing::{info, warn};

use crate::http::AppState;

use super::bridge::InboundEvent;
use super::limits::SLACK_WEBHOOK_MAX_BYTES;
use super::types::{
    SlackChannelId, SlackEventTimestamp, SlackSignature, SlackTeamId, SlackThreadTs, SlackTs,
    SlackUserId,
};
use super::verify;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/slack/events", post(handle))
        .layer(DefaultBodyLimit::max(SLACK_WEBHOOK_MAX_BYTES))
}

async fn handle(State(state): State<AppState>, headers: HeaderMap, body: Bytes) -> Response {
    let Some(slack) = state.slack.as_ref() else {
        // Feature is unconfigured — surface as 404 so a stray
        // misconfigured DNS pointing at us doesn't show as 500.
        return StatusCode::NOT_FOUND.into_response();
    };
    if let Err(status) = check_signature(slack, &headers, &body) {
        return status.into_response();
    }
    let envelope: SlackEnvelope = match serde_json::from_slice(&body) {
        Ok(e) => e,
        Err(e) => {
            warn!(error = %e, event = "slack.events.envelope_decode_failed");
            return StatusCode::BAD_REQUEST.into_response();
        }
    };
    match envelope {
        SlackEnvelope::UrlVerification { challenge } => {
            (StatusCode::OK, Json(json!({ "challenge": challenge }))).into_response()
        }
        SlackEnvelope::EventCallback { team_id, event } => {
            dispatch_event_callback(slack, team_id, event)
        }
        SlackEnvelope::Other => {
            info!(event = "slack.events.unknown_envelope");
            StatusCode::OK.into_response()
        }
    }
}

fn check_signature(
    slack: &crate::slack::SlackAppState,
    headers: &HeaderMap,
    body: &Bytes,
) -> Result<(), StatusCode> {
    let ts_header = header_str(headers, "X-Slack-Request-Timestamp")?;
    let sig_header = header_str(headers, "X-Slack-Signature")?;
    let timestamp = SlackEventTimestamp::try_from(ts_header.as_str()).map_err(|e| {
        warn!(error = %e, event = "slack.events.bad_timestamp");
        StatusCode::BAD_REQUEST
    })?;
    let signature = SlackSignature::try_from(sig_header).map_err(|e| {
        warn!(error = %e, event = "slack.events.bad_signature_shape");
        StatusCode::BAD_REQUEST
    })?;
    verify::verify(
        &slack.signing_secret,
        timestamp,
        &signature,
        body.as_ref(),
        slack.clock.now_utc(),
    )
    .map_err(|e| {
        warn!(error = ?e, event = "slack.events.verify_failed");
        StatusCode::UNAUTHORIZED
    })?;
    Ok(())
}

fn dispatch_event_callback(
    slack: &crate::slack::SlackAppState,
    team_id: String,
    event: Value,
) -> Response {
    let Some(payload) = parse_event(event) else {
        // Unknown sub-type — ack to avoid retries.
        return StatusCode::OK.into_response();
    };
    let inbound = match build_inbound(team_id, payload) {
        Ok(i) => i,
        Err(status) => return status.into_response(),
    };
    match slack.bridge_tx.try_send(inbound) {
        Ok(()) => {
            info!(event = "slack.events.enqueued");
            StatusCode::OK.into_response()
        }
        // Return 503 so Slack retries (up to 3× with exponential
        // backoff). Returning 200 here drops the event on the floor —
        // the idempotency key on retry can recover it; 200 cannot.
        Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
            warn!(event = "slack.events.bridge_queue_full");
            StatusCode::SERVICE_UNAVAILABLE.into_response()
        }
        Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
            warn!(event = "slack.events.bridge_closed");
            StatusCode::SERVICE_UNAVAILABLE.into_response()
        }
    }
}

fn build_inbound(
    team_id_raw: String,
    payload: AppMentionEvent,
) -> Result<InboundEvent, StatusCode> {
    let team_id = SlackTeamId::try_from(team_id_raw).map_err(|e| {
        warn!(error = %e, event = "slack.events.bad_team_id");
        StatusCode::BAD_REQUEST
    })?;
    let channel_id = SlackChannelId::try_from(payload.channel).map_err(|e| {
        warn!(error = %e, event = "slack.events.bad_channel_id");
        StatusCode::BAD_REQUEST
    })?;
    let user_id = SlackUserId::try_from(payload.user).map_err(|e| {
        warn!(error = %e, event = "slack.events.bad_user_id");
        StatusCode::BAD_REQUEST
    })?;
    let event_ts = SlackTs::try_from(payload.ts).map_err(|e| {
        warn!(error = %e, event = "slack.events.bad_event_ts");
        StatusCode::BAD_REQUEST
    })?;
    let thread_ts = match payload.thread_ts {
        None => None,
        Some(s) => Some(SlackThreadTs::try_from(s).map_err(|e| {
            warn!(error = %e, event = "slack.events.bad_thread_ts");
            StatusCode::BAD_REQUEST
        })?),
    };
    Ok(InboundEvent {
        team_id,
        channel_id,
        user_id,
        text: payload.text,
        thread_ts,
        event_ts,
    })
}

fn header_str(headers: &HeaderMap, name: &str) -> Result<String, StatusCode> {
    let Some(value) = headers.get(name) else {
        warn!(event = "slack.events.missing_header", header = %name);
        return Err(StatusCode::BAD_REQUEST);
    };
    let Ok(s) = value.to_str() else {
        warn!(event = "slack.events.non_ascii_header", header = %name);
        return Err(StatusCode::BAD_REQUEST);
    };
    Ok(s.to_owned())
}

// ────────────────────────────────────────────────────────────────────
// Wire envelope — minimal projection of the JSON shape we accept.
// Anything we don't read is silently dropped via the default
// `#[serde(...)]` behaviour, keeping us forward-compatible with Slack
// adding fields.
// ────────────────────────────────────────────────────────────────────

/// Top-level webhook envelope. `Other` catches forward-compatible
/// envelope types Slack may add (we ack with 200 and log).
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum SlackEnvelope {
    UrlVerification {
        challenge: String,
    },
    EventCallback {
        team_id: String,
        event: Value,
    },
    #[serde(other)]
    Other,
}

#[derive(Debug, Deserialize)]
struct AppMentionEvent {
    channel: String,
    user: String,
    text: String,
    ts: String,
    #[serde(default)]
    thread_ts: Option<String>,
    // `event_ts` exists too but equals `ts` for app_mention.
    #[allow(dead_code)]
    #[serde(default)]
    event_ts: Option<String>,
}

/// Decode `event` to `AppMentionEvent` iff `event.type == "app_mention"`.
/// Any other event sub-type returns `None`; the handler still acks 200.
fn parse_event(value: Value) -> Option<AppMentionEvent> {
    let ty = value.get("type")?.as_str()?;
    if ty != "app_mention" {
        return None;
    }
    serde_json::from_value(value).ok()
}
