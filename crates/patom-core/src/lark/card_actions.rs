//! Webhook handler — `POST /lark/card-actions` (issue #214).
//!
//! The interactive-approval callback (`card.action.trigger`). The pbbp2
//! long-connection carries event subscriptions but **not** card callbacks (per
//! the official SDK), so this is the Lark adapter's one HTTPS inbound route. The
//! shape mirrors `slack::events`: read the *raw* `Bytes`, verify before parsing,
//! and echo the `url_verification` challenge on the setup ping.
//!
//! Flow (≤3 s, HTTP 200 + JSON):
//! 1. Parse the envelope; on the `url_verification` ping echo `challenge`.
//! 2. Identify the app from the (untrusted) `header.app_id`, load its sealed
//!    card credentials — absent ⇒ 404 (fail-closed).
//! 3. Verify the request signature (Encrypt Key, over the raw body) + the body
//!    token (Verification Token), constant-time.
//! 4. Reverse-look-up the clicking `open_id` → colleague, decide via the shared
//!    `ApprovalDecider`, and respond with the resolved card (Lark swaps it in).

use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;
use tracing::{error, info, warn};

use crate::approvals::{ActionSummary, ApprovalError, ApprovalId, Decision};
use crate::http::AppState;

use super::app_store::LarkCardCredentials;
use super::limits::LARK_CARD_WEBHOOK_MAX_BYTES;
use super::types::{LarkAppId, LarkOpenId};
use super::{card, card_verify};

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/lark/card-actions", post(handle))
        .layer(DefaultBodyLimit::max(LARK_CARD_WEBHOOK_MAX_BYTES))
}

async fn handle(State(state): State<AppState>, headers: HeaderMap, body: Bytes) -> Response {
    let Some(lark) = state.lark.as_ref() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let probe: Probe = match serde_json::from_slice(&body) {
        Ok(p) => p,
        Err(e) => {
            warn!(error = %e, event = "lark.card_actions.decode_failed");
            return StatusCode::BAD_REQUEST.into_response();
        }
    };
    // 1. URL-verification setup ping — echo the challenge.
    if probe.event_type.as_deref() == Some("url_verification") {
        return probe.challenge.map_or_else(
            || StatusCode::BAD_REQUEST.into_response(),
            |c| (StatusCode::OK, Json(json!({ "challenge": c }))).into_response(),
        );
    }
    // 2. Identify the app + load its sealed card credentials (fail-closed).
    let Some(header) = probe.header.as_ref() else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let Some(app_id) = header
        .app_id
        .clone()
        .and_then(|a| LarkAppId::try_from(a).ok())
    else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let creds = match lark.apps.card_credentials(&app_id).await {
        Ok(Some(c)) => c,
        Ok(None) => return StatusCode::NOT_FOUND.into_response(),
        Err(e) => {
            warn!(error = %e, event = "lark.card_actions.unknown_app");
            return StatusCode::NOT_FOUND.into_response();
        }
    };
    // 3. Verify-before-trust: signature over the raw body + the body token.
    if !verify_request(&creds, &headers, &body, header.token.as_deref()) {
        warn!(event = "lark.card_actions.verify_failed");
        return StatusCode::UNAUTHORIZED.into_response();
    }
    // 4. Authorize the clicker, decide, render the resolved card.
    process_click(lark, &creds, &probe).await
}

/// Recompute the request signature over the raw body (Encrypt Key) and check the
/// body token (Verification Token), both constant-time.
fn verify_request(
    creds: &LarkCardCredentials,
    headers: &HeaderMap,
    body: &Bytes,
    presented_token: Option<&str>,
) -> bool {
    let (Some(ts), Some(nonce), Some(sig)) = (
        header_str(headers, "X-Lark-Request-Timestamp"),
        header_str(headers, "X-Lark-Request-Nonce"),
        header_str(headers, "X-Lark-Signature"),
    ) else {
        return false;
    };
    if !card_verify::verify_signature(creds.encrypt_key.expose(), &ts, &nonce, body.as_ref(), &sig)
    {
        return false;
    }
    let Some(token) = presented_token else {
        return false;
    };
    card_verify::verify_token(creds.verification_token.expose(), token)
}

/// Authorize the clicker via the shadow directory, decide through the shared
/// seam, and respond with the resolved card (Lark swaps it in place).
async fn process_click(
    lark: &super::LarkAppState,
    creds: &LarkCardCredentials,
    probe: &Probe,
) -> Response {
    let Some((approval_id, decision, open_id)) = parse_action(probe) else {
        // Not our approval button (or a malformed value) — ack with no change.
        return StatusCode::OK.into_response();
    };
    let org_id = creds.org_id;
    let clicker = match lark.directory.colleague_for_open_id(org_id, &open_id).await {
        Ok(Some(c)) => c,
        Ok(None) => return toast("error", "We couldn't match your Lark account."),
        Err(e) => {
            error!(error = %e, event = "lark.card_actions.clicker_lookup_failed");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };
    match lark
        .decider
        .decide(org_id, approval_id, decision, clicker)
        .await
    {
        Ok(outcome) => {
            let record = outcome.record();
            let name = decider_name(lark, org_id, clicker).await;
            info!(patom.approval.id = %approval_id, event = "lark.card_actions.decided");
            resolved_response(decision, &name, &record.action_summary)
        }
        Err(ApprovalError::Unauthorized) => {
            toast("error", "You're not authorized to decide this approval.")
        }
        Err(ApprovalError::NotFound | ApprovalError::NotPending | ApprovalError::Expired) => {
            toast("info", "This approval is no longer open.")
        }
        Err(e) => {
            error!(error = %e, event = "lark.card_actions.decide_failed");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

/// Extract `(approval_id, decision, open_id)` from a verified callback, or `None`
/// for a non-approval / malformed action.
fn parse_action(probe: &Probe) -> Option<(ApprovalId, Decision, LarkOpenId)> {
    let event = probe.event.as_ref()?;
    let open_id = event.operator.as_ref()?.open_id.as_ref()?;
    let value = event.action.as_ref()?.value.as_ref()?;
    let approval_id = ApprovalId::try_from(value.approval_id.as_deref()?).ok()?;
    let decision = Decision::from_tag(value.decision.as_deref()?)?;
    let open_id = LarkOpenId::try_from(open_id.clone()).ok()?;
    Some((approval_id, decision, open_id))
}

/// The clicker's display name for the resolved card, falling back to a generic
/// label when the colleague has no resolvable handle.
async fn decider_name(
    lark: &super::LarkAppState,
    org_id: crate::auth::OrgId,
    clicker: crate::colleagues::ColleagueId,
) -> String {
    match lark.directory.tag_for(org_id, clicker).await {
        Ok(Some((name, _))) => name,
        Ok(None) | Err(_) => "a colleague".to_owned(),
    }
}

/// `{"toast": …, "card": {"type":"raw","data": <resolved>}}` — Lark replaces the
/// card with the resolved view and shows a confirmation toast.
fn resolved_response(decision: Decision, decider_name: &str, action: &ActionSummary) -> Response {
    let resolved = card::resolved_card(decision, decider_name, action);
    let (toast_type, toast_text) = match decision {
        Decision::Approved => ("success", "Approved"),
        Decision::Denied => ("info", "Denied"),
    };
    (
        StatusCode::OK,
        Json(json!({
            "toast": { "type": toast_type, "content": toast_text },
            "card": { "type": "raw", "data": resolved },
        })),
    )
        .into_response()
}

fn toast(toast_type: &str, content: &str) -> Response {
    (
        StatusCode::OK,
        Json(json!({ "toast": { "type": toast_type, "content": content } })),
    )
        .into_response()
}

fn header_str(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned)
}

// ────────────────────────────────────────────────────────────────────
// Wire envelope — minimal projection (forward-compatible: unread fields drop).
// ────────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct Probe {
    /// Present on the `url_verification` setup ping.
    #[serde(default, rename = "type")]
    event_type: Option<String>,
    #[serde(default)]
    challenge: Option<String>,
    #[serde(default)]
    header: Option<Header>,
    #[serde(default)]
    event: Option<CardEvent>,
}

#[derive(Debug, Deserialize)]
struct Header {
    #[serde(default)]
    app_id: Option<String>,
    /// The Verification Token, echoed in the v2 event header.
    #[serde(default)]
    token: Option<String>,
}

#[derive(Debug, Deserialize)]
struct CardEvent {
    #[serde(default)]
    operator: Option<Operator>,
    #[serde(default)]
    action: Option<CardAction>,
}

#[derive(Debug, Deserialize)]
struct Operator {
    #[serde(default)]
    open_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct CardAction {
    #[serde(default)]
    value: Option<ActionValue>,
}

#[derive(Debug, Deserialize)]
struct ActionValue {
    #[serde(default)]
    approval_id: Option<String>,
    #[serde(default)]
    decision: Option<String>,
}
