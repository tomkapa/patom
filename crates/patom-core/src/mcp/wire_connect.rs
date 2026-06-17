//! Plain-text rendering for an agent's MCP connection request in chat.
//!
//! Lark and Discord both deliver `ResponseChunk::WireMcpRequest` as a plain
//! message (no interactive card — unlike Slack's Block Kit peer in
//! `slack/connection_card.rs`), so they render the request identically: a lead
//! line with the provider + reason, then either a signed connect link (OAuth2)
//! or a "finish in the web UI" pointer. This module is that shared renderer so
//! the two stream pumps don't carry a copy each.
//
// `redundant_pub_crate`: items are `pub(crate)` so the lark/discord pumps in
// sibling modules can reach them; clippy flags that as redundant against the
// `pub(crate) mod` declaration, but dropping to bare `pub` trips
// `unreachable_pub`. Allow the nursery lint to keep the module crate-internal.
#![allow(clippy::redundant_pub_crate)]

use crate::mcp::McpAuthKind;

/// Maximum byte length of a signed chat connect token at the unauthenticated
/// `GET /{platform}/mcp/connect` boundary (CLAUDE.md §5 — every string crossing
/// a trust boundary has a length cap). A real token is ~450 bytes (colon-joined
/// ids + UUIDs + a 64-char HMAC tail); this is generous head-room that still
/// rejects an oversized hostile query before any HMAC work. Shared by the Lark
/// and Discord `verify_connect`.
pub(crate) const CONNECT_TOKEN_MAX_BYTES: usize = 1024;

/// Render the plain-text connect message a chat pump posts for a
/// `WireMcpRequest`.
///
/// With an OAuth2 catalog and a minted `url` the message carries the connect
/// link; otherwise (static-headers / no-auth, or a defensively-missing url) it
/// points the user at the web UI — neither Lark nor Discord can host a
/// secret-entry form. `reason` is truncated to `reason_max_chars` (the cap
/// differs per platform: Discord's hard 2000-char message limit forces a
/// tighter bound than Lark).
#[must_use]
pub(crate) fn render_connect_message(
    display_name: &str,
    reason: &str,
    auth_kind: McpAuthKind,
    url: Option<&str>,
    reason_max_chars: usize,
) -> String {
    let reason = truncate_with_ellipsis(reason.to_owned(), reason_max_chars);
    let lead = format!("🔌 {display_name} — {reason}");
    let tail = match (auth_kind, url) {
        (McpAuthKind::OAuth2, Some(u)) => format!("Open Patom to connect: {u}"),
        _ => format!("Finish wiring {display_name} in the Patom web UI."),
    };
    format!("{lead}\n{tail}")
}

/// Truncate `s` to at most `max_chars` characters, appending an ellipsis if
/// truncated. Slices on a char boundary so a UTF-8 codepoint is never split.
/// Takes the `String` by value so the within-bound case is a move, not a copy.
#[must_use]
pub(crate) fn truncate_with_ellipsis(s: String, max_chars: usize) -> String {
    if max_chars == 0 {
        return String::new();
    }
    if s.chars().count() <= max_chars {
        return s;
    }
    let mut out: String = s.chars().take(max_chars.saturating_sub(1)).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oauth2_carries_link() {
        let msg = render_connect_message(
            "Notion",
            "draft the brief",
            McpAuthKind::OAuth2,
            Some("https://patom.example/lark/mcp/connect?token=abc"),
            2_000,
        );
        assert!(msg.contains("Notion"));
        assert!(msg.contains("draft the brief"));
        assert!(msg.contains("https://patom.example/lark/mcp/connect?token=abc"));
        assert!(msg.contains("Open Patom to connect"));
    }

    #[test]
    fn static_headers_points_to_web_ui() {
        let msg = render_connect_message(
            "Internal",
            "need access",
            McpAuthKind::StaticHeaders,
            None,
            2_000,
        );
        assert!(msg.contains("Patom web UI"));
        assert!(!msg.contains("/mcp/connect"));
    }

    #[test]
    fn oauth2_without_url_degrades_to_web_ui() {
        // Defensive: an OAuth2 catalog should always get a url, but never emit a
        // dangling "Open Patom to connect:" with nothing after it.
        let msg = render_connect_message("Notion", "why", McpAuthKind::OAuth2, None, 2_000);
        assert!(msg.contains("Patom web UI"));
        assert!(!msg.contains("Open Patom to connect"));
    }

    #[test]
    fn long_reason_is_truncated() {
        let long = "a".repeat(1_500);
        let msg = render_connect_message("Notion", &long, McpAuthKind::None, None, 1_400);
        assert!(msg.contains('…'));
        // The lead carries the truncated reason; the whole message stays bounded.
        assert!(msg.chars().count() <= 1_400 + 60);
    }

    #[test]
    fn truncate_trims_with_ellipsis() {
        let out = truncate_with_ellipsis("a".repeat(100), 10);
        assert_eq!(out.chars().count(), 10);
        assert!(out.ends_with('…'));
    }

    #[test]
    fn truncate_leaves_short_input_untouched() {
        assert_eq!(truncate_with_ellipsis("short".to_owned(), 10), "short");
    }

    #[test]
    fn truncate_on_char_boundary_for_multibyte() {
        let s = "é".repeat(20);
        let out = truncate_with_ellipsis(s, 10);
        // Valid UTF-8, counted to the cap — never splits a codepoint.
        assert_eq!(out.chars().count(), 10);
    }
}
