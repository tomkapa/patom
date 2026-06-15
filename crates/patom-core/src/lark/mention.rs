//! Pure Lark mention parsing + outbound `<at>` rendering.
//!
//! A Lark `im.message.receive_v1` text event renders mentions as placeholder
//! tokens (`@_user_1`, `@_user_2`, …) in the message body, with a parallel
//! `mentions` array mapping each placeholder `key` to an id set
//! (`open_id`/`user_id`/`union_id`) plus a display name. Unlike Slack — one app
//! serving many agents via `@AgentName` — a Lark app **is** one bot == one
//! agent, so there is no agent-name sub-parsing: a mention of *the bot* is the
//! trigger, and the receiving agent is the app's agent (resolved by the bridge
//! from `lark_apps`).
//!
//! This module only: (a) tells whether the bot is mentioned, (b) strips the
//! bot's `@_user_N` placeholder(s) from the text to yield the clean prompt
//! body, and (c) renders outbound `<at>` markup.
//!
//! Parser-only — no DB, no async, no I/O. Identity resolution and routing live
//! in `bridge.rs`.

use super::types::LarkOpenId;

/// One entry of the event's `mentions` array, reduced to what routing needs.
///
/// `key` is the in-text placeholder (`@_user_1`); `open_id` is the per-app
/// satellite id when Lark supplied one for this mention (absent for
/// `@_all`-style or id-suppressed mentions).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AtToken {
    /// The in-text placeholder this mention substitutes for (`@_user_1`).
    pub key: String,
    /// The mentioned party's `open_id`, when present in the event.
    pub open_id: Option<String>,
    /// The mentioned party's display name from the event, for inbound rendering
    /// (the `@_user_N` placeholder → `@<name>` so the agent reads who is meant).
    pub name: String,
}

/// True iff one of the message's mentions resolves to `bot_open_id`.
#[must_use]
pub fn mentions_bot(tokens: &[AtToken], bot_open_id: &str) -> bool {
    tokens
        .iter()
        .any(|t| t.open_id.as_deref() == Some(bot_open_id))
}

/// Remove the bot's `@_user_N` placeholder(s) from `text`, returning the clean
/// prompt body.
///
/// Finds every key whose `open_id` equals `bot_open_id`, deletes each occurrence
/// of those key substrings from `text`, collapses the runs of whitespace left
/// behind, and trims the ends. Other parties' placeholders are preserved
/// verbatim so the bridge can resolve them downstream.
#[must_use]
pub fn strip_bot_mentions(text: &str, tokens: &[AtToken], bot_open_id: &str) -> String {
    let mut out = text.to_owned();
    for token in tokens {
        if token.open_id.as_deref() == Some(bot_open_id) {
            out = out.replace(&token.key, "");
        }
    }
    collapse_whitespace(&out)
}

/// Render an inbound message's mention placeholders to readable `@Name` text
/// (and strip the bot's own trigger mention), so the agent reads who is
/// referenced — mirroring the web feed's `@Name` form.
///
/// Each event mention is an `@_user_N` placeholder in `text` with a parallel
/// `name`. The bot's placeholder is removed (it's the trigger marker); every
/// other becomes `@<name>`; a placeholder with no name is dropped. Whitespace
/// left behind is collapsed.
#[must_use]
pub fn render_inbound(text: &str, tokens: &[AtToken], bot_open_id: &str) -> String {
    let mut out = text.to_owned();
    for token in tokens {
        // The bot's own placeholder (the trigger marker) and a name-less mention
        // both drop to empty; any other becomes the readable `@<name>`.
        let replacement = if token.open_id.as_deref() == Some(bot_open_id) || token.name.is_empty()
        {
            String::new()
        } else {
            format!("@{}", token.name)
        };
        out = out.replace(&token.key, &replacement);
    }
    collapse_whitespace(&out)
}

/// Render an outbound text/post mention as `<at user_id="ou_…">name</at>`.
///
/// The `open_id` is the safe id form for `@`-tagging a single user. The output
/// is HTML-ish markup; the name is inserted verbatim (Lark renders the supplied
/// id and ignores a mismatched name).
#[must_use]
pub fn render_at(open_id: &LarkOpenId, name: &str) -> String {
    format!("<at user_id=\"{}\">{}</at>", open_id.as_str(), name)
}

/// Render the everyone-mention: `<at id=all></at>`.
#[must_use]
pub fn render_at_all() -> String {
    "<at id=all></at>".to_owned()
}

/// Rewrite `@Name` mentions in an outbound reply into Lark `<at>` markup so the
/// addressed colleague is actually pinged.
///
/// `handles` is `(colleague_display_name, open_id)` for the org's Lark humans.
/// Mirrors the web UI's `findNamedMentions`: matches `@Name` only at a word
/// boundary (start-of-text or after whitespace — so `a@b.com` is not a
/// mention), longest name first (so `Tom Tran` beats `Tom`), and only when the
/// char after the name is a non-name boundary. A name with no Lark handle (an
/// agent/bot name, or an unknown) is left as plain `@Name` text.
#[must_use]
pub fn render_ats(content: &str, handles: &[(String, LarkOpenId)]) -> String {
    if handles.is_empty() {
        return content.to_owned();
    }
    let mut sorted: Vec<&(String, LarkOpenId)> =
        handles.iter().filter(|h| !h.0.is_empty()).collect();
    // Longest name first, so "Tom Tran" wins over "Tom".
    sorted.sort_by_key(|h| std::cmp::Reverse(h.0.len()));

    let mut out = String::with_capacity(content.len());
    let mut rest = content;
    // Whether the char before `rest` is a boundary (whitespace / start-of-text).
    let mut prev_is_boundary = true;
    while let Some(ch) = rest.chars().next() {
        if ch == '@' && prev_is_boundary {
            let after_at = &rest['@'.len_utf8()..];
            if let Some(h) = sorted.iter().find(|h| {
                after_at.starts_with(h.0.as_str())
                    && after_at[h.0.len()..]
                        .chars()
                        .next()
                        .is_none_or(|c| !is_name_char(c))
            }) {
                out.push_str(&render_at(&h.1, &h.0));
                rest = &after_at[h.0.len()..];
                // The next char follows a name char, so a `@` there is not a
                // boundary mention.
                prev_is_boundary = false;
                continue;
            }
        }
        out.push(ch);
        prev_is_boundary = ch.is_whitespace();
        rest = &rest[ch.len_utf8()..];
    }
    out
}

/// A char that can appear inside a mentionable name run (`[A-Za-z0-9_-]`); the
/// name match requires a non-name char (or end) immediately after.
fn is_name_char(c: char) -> bool {
    c.is_alphanumeric() || c == '-' || c == '_'
}

/// Collapse internal whitespace runs to a single space and trim the ends.
///
/// Deleting a placeholder leaves a double space (`"a  b"`); this normalises it
/// to the single space a reader would have typed.
fn collapse_whitespace(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    const BOT: &str = "ou_bot";

    fn tok(key: &str, open_id: Option<&str>) -> AtToken {
        AtToken {
            key: key.to_owned(),
            open_id: open_id.map(str::to_owned),
            name: String::new(),
        }
    }

    fn tok_named(key: &str, open_id: Option<&str>, name: &str) -> AtToken {
        AtToken {
            key: key.to_owned(),
            open_id: open_id.map(str::to_owned),
            name: name.to_owned(),
        }
    }

    fn bot_open_id() -> LarkOpenId {
        LarkOpenId::try_from("ou_x").expect("valid open id")
    }

    #[test]
    fn mentions_bot_true_when_open_id_matches() {
        let tokens = [
            tok("@_user_1", Some("ou_alice")),
            tok("@_user_2", Some(BOT)),
        ];
        assert!(mentions_bot(&tokens, BOT));
    }

    #[test]
    fn mentions_bot_false_when_no_match() {
        let tokens = [
            tok("@_user_1", Some("ou_alice")),
            tok("@_user_2", Some("ou_bob")),
        ];
        assert!(!mentions_bot(&tokens, BOT));
    }

    #[test]
    fn mentions_bot_false_on_empty_tokens() {
        assert!(!mentions_bot(&[], BOT));
    }

    #[test]
    fn mentions_bot_ignores_missing_open_id() {
        let tokens = [tok("@_all", None)];
        assert!(!mentions_bot(&tokens, BOT));
    }

    #[test]
    fn strip_removes_bot_placeholder_keeps_others() {
        let tokens = [
            tok("@_user_1", Some(BOT)),
            tok("@_user_2", Some("ou_alice")),
        ];
        let cleaned = strip_bot_mentions("@_user_1 @_user_2 ship it", &tokens, BOT);
        assert_eq!(cleaned, "@_user_2 ship it");
    }

    #[test]
    fn strip_bare_bot_mention_yields_empty() {
        let tokens = [tok("@_user_1", Some(BOT))];
        let cleaned = strip_bot_mentions("@_user_1", &tokens, BOT);
        assert_eq!(cleaned, "");
    }

    #[test]
    fn strip_collapses_internal_whitespace() {
        // Bot mention sits between two words; removing it must not leave a
        // double space.
        let tokens = [tok("@_user_1", Some(BOT))];
        let cleaned = strip_bot_mentions("hello @_user_1 world", &tokens, BOT);
        assert_eq!(cleaned, "hello world");
    }

    #[test]
    fn strip_removes_repeated_bot_placeholder() {
        // The same bot placeholder can appear twice in one message.
        let tokens = [tok("@_user_1", Some(BOT))];
        let cleaned = strip_bot_mentions("@_user_1 then @_user_1 again", &tokens, BOT);
        assert_eq!(cleaned, "then again");
    }

    #[test]
    fn strip_with_no_bot_token_passes_text_through() {
        let tokens = [tok("@_user_1", Some("ou_alice"))];
        let cleaned = strip_bot_mentions("@_user_1 hi there", &tokens, BOT);
        assert_eq!(cleaned, "@_user_1 hi there");
    }

    #[test]
    fn strip_trims_surrounding_whitespace() {
        let tokens = [tok("@_user_1", Some(BOT))];
        let cleaned = strip_bot_mentions("  @_user_1   summarise this  ", &tokens, BOT);
        assert_eq!(cleaned, "summarise this");
    }

    #[test]
    fn render_at_produces_exact_markup() {
        let rendered = render_at(&bot_open_id(), "Alice");
        assert_eq!(rendered, "<at user_id=\"ou_x\">Alice</at>");
    }

    #[test]
    fn render_at_inserts_name_verbatim() {
        let rendered = render_at(&bot_open_id(), "Data Eng");
        assert_eq!(rendered, "<at user_id=\"ou_x\">Data Eng</at>");
    }

    #[test]
    fn render_at_all_is_fixed_markup() {
        assert_eq!(render_at_all(), "<at id=all></at>");
    }

    #[test]
    fn render_inbound_strips_bot_and_names_others() {
        let tokens = [
            tok_named("@_user_1", Some(BOT), "Recruiter"),
            tok_named("@_user_2", Some("ou_test"), "Test User"),
        ];
        let out = render_inbound("@_user_1 transfer hello to @_user_2", &tokens, BOT);
        assert_eq!(out, "transfer hello to @Test User");
    }

    #[test]
    fn render_inbound_drops_nameless_placeholder() {
        let tokens = [tok("@_user_2", Some("ou_x"))]; // no name
        let out = render_inbound("ping @_user_2 now", &tokens, BOT);
        assert_eq!(out, "ping now");
    }

    fn oid(s: &str) -> LarkOpenId {
        LarkOpenId::try_from(s).expect("valid open id")
    }

    #[test]
    fn render_ats_rewrites_known_names_longest_first() {
        let handles = vec![
            ("Tom".to_owned(), oid("ou_tom")),
            ("Tom Tran".to_owned(), oid("ou_tomtran")),
        ];
        let out = render_ats("hi @Tom Tran and @Tom, also a@b.com and @Unknown", &handles);
        assert_eq!(
            out,
            "hi <at user_id=\"ou_tomtran\">Tom Tran</at> and \
             <at user_id=\"ou_tom\">Tom</at>, also a@b.com and @Unknown"
        );
    }

    #[test]
    fn render_ats_passthrough_when_no_handles_or_no_match() {
        assert_eq!(render_ats("@Tom hi", &[]), "@Tom hi");
        let handles = vec![("Alice".to_owned(), oid("ou_alice"))];
        assert_eq!(render_ats("@Bob hi", &handles), "@Bob hi");
    }

    #[test]
    fn render_ats_only_at_word_boundary() {
        let handles = vec![("Tom".to_owned(), oid("ou_tom"))];
        // Not preceded by whitespace → not a mention.
        assert_eq!(render_ats("email me@Tom", &handles), "email me@Tom");
        // `@Tomas` must not match `Tom` (next char is a name char).
        assert_eq!(render_ats("hey @Tomas", &handles), "hey @Tomas");
    }
}
