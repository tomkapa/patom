//! Block Kit JSON builder for the agent's MCP connection-request card.
//!
//! Renders a [`ResponseChunk::WireMcpRequest`] event into the Block Kit
//! payload posted into the Slack thread. The web UI renders the same
//! event via `WireMcpRequestCard.tsx`; this module is the Slack-side
//! peer.
//!
//! Pure — no async, no IO, no DB. Snapshot-style tests assert the JSON
//! field-by-field (no `insta` dep in the tree).
//!
//! ## Shape
//!
//! Returns a bare `serde_json::Value::Array` — the value Slack expects
//! in `chat.postMessage`'s `blocks` field. The caller wraps it into a
//! `PostBody::Blocks { fallback_text, blocks: <this> }`; the poster
//! places it directly under the wire `blocks` field. **Do not wrap in
//! `{ "blocks": [...] }`** — that nests under `body.blocks` and Slack
//! silently rejects the post.
//!
//! - One **section** block with the recruiter's `reason` paragraph
//!   (truncated to [`SLACK_CONNECTION_REASON_MAX_CHARS`]).
//! - For `auth_kind = oauth2`: one **actions** block with a primary
//!   `Connect <Provider>` button whose `url` field points at the signed
//!   `GET /slack/mcp/connect` endpoint. The button has no `action_id` —
//!   Slack does not invoke our interactivity handler for `url` buttons.
//! - For `auth_kind = static_headers | none`: a **context** block
//!   pointing the user back to the web UI (Slack can't host the
//!   provider's secret-entry form). Matches the FE's behaviour in
//!   `web/src/components/organisms/WireMcpRequestCard.tsx`.
//!
//! Fallback text for the post is the caller's responsibility — Slack
//! requires one for notifications and accessibility (see
//! `PostBody::Blocks { fallback_text, .. }`).

use serde_json::{Value, json};

use crate::mcp::McpAuthKind;

use super::limits::SLACK_CONNECTION_REASON_MAX_CHARS;

/// Build the Block Kit payload for an MCP connection request.
///
/// `reason` — recruiter-supplied paragraph explaining why this wiring
/// is being asked for. Truncated with an ellipsis at
/// [`SLACK_CONNECTION_REASON_MAX_CHARS`] characters.
///
/// `display_name` — human label for the provider (`"Notion"`, `"Linear"`).
///
/// `auth_kind` — chooses between the button + URL flow (`oauth2`) and
/// the "finish in the web UI" hint (`static_headers` / `none`).
///
/// `connect_url` — signed URL the button opens, only consulted when
/// `auth_kind == OAuth2`. When `None` for an oauth2 card we degrade to
/// the same context-block hint as the non-oauth2 path (it shouldn't
/// happen — caller is expected to mint the URL when oauth2 — but the
/// degradation keeps the card from ever rendering an unclickable button).
#[must_use]
pub fn build_connection_request_card(
    reason: &str,
    display_name: &str,
    auth_kind: McpAuthKind,
    connect_url: Option<&str>,
) -> Value {
    let truncated_reason = truncate(reason, SLACK_CONNECTION_REASON_MAX_CHARS);
    let section = json!({
        "type": "section",
        "text": {
            "type": "mrkdwn",
            "text": truncated_reason,
        },
    });

    let mut blocks = Vec::with_capacity(2);
    blocks.push(section);

    match (auth_kind, connect_url) {
        (McpAuthKind::OAuth2, Some(url)) => {
            blocks.push(json!({
                "type": "actions",
                "elements": [{
                    "type": "button",
                    "style": "primary",
                    "text": {
                        "type": "plain_text",
                        "text": format!("Connect {display_name}"),
                    },
                    "url": url,
                }],
            }));
        }
        _ => {
            // Non-oauth2 (static headers / none) or a missing url —
            // point the user back to the web UI. Slack can't host the
            // secret-entry form for static-headers integrations.
            blocks.push(json!({
                "type": "context",
                "elements": [{
                    "type": "mrkdwn",
                    "text": format!(
                        "_Finish wiring *{display_name}* in the Relay web UI._"
                    ),
                }],
            }));
        }
    }

    Value::Array(blocks)
}

/// Truncate `s` to at most `max_chars` characters, appending an ellipsis
/// if truncated. Slices on a char boundary so we never split a UTF-8
/// codepoint. Returns the input unchanged when within bound.
fn truncate(s: &str, max_chars: usize) -> String {
    if max_chars == 0 {
        return String::new();
    }
    if s.chars().count() <= max_chars {
        return s.to_owned();
    }
    let mut out: String = s.chars().take(max_chars.saturating_sub(1)).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oauth2_card_has_section_and_primary_button() {
        let v = build_connection_request_card(
            "I want to draft a recruiting brief in Notion before the kickoff.",
            "Notion",
            McpAuthKind::OAuth2,
            Some("https://relay.example/slack/mcp/connect?token=abc"),
        );
        let blocks = v.as_array().expect("blocks array");
        assert_eq!(blocks.len(), 2, "oauth2 card has section + actions");

        // Section: type + mrkdwn reason.
        assert_eq!(blocks[0]["type"], "section");
        assert_eq!(blocks[0]["text"]["type"], "mrkdwn");
        assert!(
            blocks[0]["text"]["text"]
                .as_str()
                .expect("reason string")
                .contains("recruiting brief")
        );

        // Actions: one primary button labelled "Connect Notion" with the url field.
        assert_eq!(blocks[1]["type"], "actions");
        let elements = blocks[1]["elements"].as_array().expect("elements array");
        assert_eq!(elements.len(), 1);
        let btn = &elements[0];
        assert_eq!(btn["type"], "button");
        assert_eq!(btn["style"], "primary");
        assert_eq!(btn["text"]["type"], "plain_text");
        assert_eq!(btn["text"]["text"], "Connect Notion");
        assert_eq!(
            btn["url"],
            "https://relay.example/slack/mcp/connect?token=abc"
        );
        assert!(
            btn.get("action_id").is_none(),
            "url-style button has no action_id (we don't subscribe to a callback)"
        );
    }

    #[test]
    fn static_headers_card_renders_context_hint_not_button() {
        let v = build_connection_request_card(
            "Need Linear access to query the issue ID for this ticket.",
            "Linear",
            McpAuthKind::StaticHeaders,
            None,
        );
        let blocks = v.as_array().expect("blocks");
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0]["type"], "section");
        assert_eq!(blocks[1]["type"], "context");
        let hint = blocks[1]["elements"][0]["text"]
            .as_str()
            .expect("hint text");
        assert!(hint.contains("Linear"));
        assert!(hint.contains("Relay web UI"));
    }

    #[test]
    fn auth_kind_none_renders_context_hint_not_button() {
        let v = build_connection_request_card(
            "No auth needed but FE setup still required.",
            "Internal",
            McpAuthKind::None,
            // Even if a url is supplied for a None-auth kind we render the hint —
            // there is no consent flow to drive the user into.
            Some("https://relay.example/slack/mcp/connect?token=xyz"),
        );
        let blocks = v.as_array().expect("blocks");
        assert_eq!(blocks[1]["type"], "context");
    }

    #[test]
    fn oauth2_without_url_degrades_to_hint() {
        // Defensive: even though stream_pump always mints a url for oauth2 catalogs,
        // ensure the renderer never emits a button without a url.
        let v = build_connection_request_card(
            "OAuth2 catalog but caller passed None.",
            "Notion",
            McpAuthKind::OAuth2,
            None,
        );
        let blocks = v.as_array().expect("blocks");
        assert_eq!(blocks[1]["type"], "context");
    }

    #[test]
    fn long_reason_is_truncated_with_ellipsis() {
        let long = "a".repeat(SLACK_CONNECTION_REASON_MAX_CHARS + 500);
        let v = build_connection_request_card(
            &long,
            "Notion",
            McpAuthKind::OAuth2,
            Some("https://relay.example/x"),
        );
        let rendered = v[0]["text"]["text"].as_str().expect("text string");
        assert_eq!(rendered.chars().count(), SLACK_CONNECTION_REASON_MAX_CHARS);
        assert!(rendered.ends_with('…'));
    }

    #[test]
    fn short_reason_is_unchanged() {
        let v = build_connection_request_card(
            "short",
            "Notion",
            McpAuthKind::OAuth2,
            Some("https://relay.example/x"),
        );
        assert_eq!(v[0]["text"]["text"], "short");
    }

    #[test]
    fn unicode_reason_is_truncated_on_char_boundary() {
        // Multi-byte characters; ensure truncation never splits a codepoint.
        let s: String = "é".repeat(SLACK_CONNECTION_REASON_MAX_CHARS + 10);
        let v = build_connection_request_card(
            &s,
            "Notion",
            McpAuthKind::OAuth2,
            Some("https://relay.example/x"),
        );
        let rendered = v[0]["text"]["text"].as_str().expect("text string");
        // Must still be valid UTF-8 and char-counted to the cap.
        assert_eq!(rendered.chars().count(), SLACK_CONNECTION_REASON_MAX_CHARS);
    }

    #[test]
    fn special_chars_in_reason_do_not_break_json() {
        let reason = "He said \"hello\" — and { wrote: 'json' }\n\twith tabs.";
        let v = build_connection_request_card(
            reason,
            "Notion",
            McpAuthKind::OAuth2,
            Some("https://relay.example/x"),
        );
        // The whole structure must round-trip through serde.
        let s = serde_json::to_string(&v).expect("serialise");
        let back: Value = serde_json::from_str(&s).expect("parse back");
        assert_eq!(back[0]["text"]["text"], reason);
    }
}
