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

use super::bridge::{InboundEvent, InboundSource};
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

/// Verify the HMAC signature on an inbound Slack webhook. Shared with
/// [`super::interactions`] — every Slack webhook uses the same header
/// pair and the same signing secret, so the extraction + verify glue
/// has no reason to be per-module.
pub(super) fn check_signature(
    slack: &crate::slack::SlackAppState,
    headers: &HeaderMap,
    body: &Bytes,
) -> Result<(), StatusCode> {
    let ts_header = header_str(headers, "X-Slack-Request-Timestamp")?;
    let sig_header = header_str(headers, "X-Slack-Signature")?;
    let timestamp = SlackEventTimestamp::try_from(ts_header.as_str()).map_err(|e| {
        warn!(error = %e, event = "slack.webhook.bad_timestamp");
        StatusCode::BAD_REQUEST
    })?;
    let signature = SlackSignature::try_from(sig_header).map_err(|e| {
        warn!(error = %e, event = "slack.webhook.bad_signature_shape");
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
        warn!(error = ?e, event = "slack.webhook.verify_failed");
        StatusCode::UNAUTHORIZED
    })?;
    Ok(())
}

fn dispatch_event_callback(
    slack: &crate::slack::SlackAppState,
    team_id: String,
    event: Value,
) -> Response {
    let Some((source, payload)) = parse_event(event) else {
        // Unknown sub-type or one we deliberately filter (bot
        // echo, message_changed, top-level chatter without
        // thread_ts, …) — ack to avoid retries.
        return StatusCode::OK.into_response();
    };
    let inbound = match build_inbound(team_id, payload, source) {
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
    source: InboundSource,
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
        source,
    })
}

pub(super) fn header_str(headers: &HeaderMap, name: &str) -> Result<String, StatusCode> {
    let Some(value) = headers.get(name) else {
        warn!(event = "slack.webhook.missing_header", header = %name);
        return Err(StatusCode::BAD_REQUEST);
    };
    let Ok(s) = value.to_str() else {
        warn!(event = "slack.webhook.non_ascii_header", header = %name);
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

/// Decode an inbound event envelope to `(InboundSource, AppMentionEvent)`.
///
/// Two event types are routed:
///
/// - `app_mention` — the bot was `@`-mentioned. Always processed by
///   the bridge, falling back to the default agent on an unresolvable
///   name.
/// - `message` — a non-mention message in a channel. Only processed
///   if it is a reply inside an existing thread (`thread_ts` set) and
///   carries no `subtype` (i.e. is a normal user message, not a
///   bot-message echo, edit, deletion, channel-join, etc.) and no
///   `bot_id` (Slack stamps bot-attributed posts with one even when
///   `subtype` is absent). The bridge drops the event further if the
///   thread has no `slack_threads` binding.
///
/// Any other event type — or a `message` event we filtered — returns
/// `None`; the handler still acks 200 so Slack doesn't retry.
fn parse_event(value: Value) -> Option<(InboundSource, AppMentionEvent)> {
    let ty = value.get("type")?.as_str()?;
    match ty {
        "app_mention" => {
            let m: AppMentionEvent = serde_json::from_value(value).ok()?;
            Some((InboundSource::AppMention, m))
        }
        "message" => {
            // Filter out edits, deletions, channel joins, file shares,
            // bot-message echoes, etc. Any non-empty `subtype` means
            // this is not a plain user message.
            if value.get("subtype").and_then(Value::as_str).is_some() {
                return None;
            }
            // Slack stamps bot-attributed posts with `bot_id` even
            // when `subtype` is missing — drop those too to avoid the
            // bot reacting to itself.
            if value.get("bot_id").and_then(Value::as_str).is_some() {
                return None;
            }
            // Only react to thread replies; top-level channel chatter
            // is not a Patom surface.
            value.get("thread_ts").and_then(Value::as_str)?;
            let m: AppMentionEvent = serde_json::from_value(value).ok()?;
            Some((InboundSource::ThreadMessage, m))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(extra: Value) -> Value {
        let mut base = json!({
            "type": "message",
            "channel": "C1",
            "user": "U1",
            "text": "hi",
            "ts": "1700000000.000100",
        });
        if let Value::Object(ref mut m) = base
            && let Value::Object(extra_map) = extra
        {
            for (k, v) in extra_map {
                m.insert(k, v);
            }
        }
        base
    }

    #[test]
    fn parse_app_mention_routes_to_appmention_source() {
        let v = json!({
            "type": "app_mention",
            "channel": "C1",
            "user": "U1",
            "text": "<@U_BOT> hi",
            "ts": "1700000000.000100",
        });
        let (source, _) = parse_event(v).expect("app_mention");
        assert_eq!(source, InboundSource::AppMention);
    }

    #[test]
    fn parse_message_routes_thread_reply_to_threadmessage_source() {
        let v = msg(json!({ "thread_ts": "1700000000.000050" }));
        let (source, m) = parse_event(v).expect("thread message");
        assert_eq!(source, InboundSource::ThreadMessage);
        assert_eq!(m.thread_ts.as_deref(), Some("1700000000.000050"));
    }

    #[test]
    fn parse_message_drops_top_level_chatter() {
        // No thread_ts → not a Patom surface, drop.
        let v = msg(json!({}));
        assert!(parse_event(v).is_none());
    }

    #[test]
    fn parse_message_drops_bot_message_subtype() {
        // Slack stamps the bot's own posts with subtype="bot_message".
        let v = msg(json!({
            "thread_ts": "1700000000.000050",
            "subtype": "bot_message",
        }));
        assert!(parse_event(v).is_none());
    }

    #[test]
    fn parse_message_drops_bot_id_messages() {
        // Some bot posts carry bot_id without subtype.
        let v = msg(json!({
            "thread_ts": "1700000000.000050",
            "bot_id": "B12345",
        }));
        assert!(parse_event(v).is_none());
    }

    #[test]
    fn parse_message_drops_edits_and_deletions() {
        // Any non-empty subtype is filtered (edits, deletions,
        // channel joins, file shares, …).
        for sub in &["message_changed", "message_deleted", "channel_join"] {
            let v = msg(json!({
                "thread_ts": "1700000000.000050",
                "subtype": *sub,
            }));
            assert!(parse_event(v).is_none(), "subtype {sub} not filtered");
        }
    }

    #[test]
    fn parse_unknown_type_returns_none() {
        let v = json!({ "type": "reaction_added" });
        assert!(parse_event(v).is_none());
    }
}
