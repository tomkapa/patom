//! Pure mention parser.
//!
//! Given the raw `text` of an `app_mention` event plus the bot's Slack
//! user ID, extract the *first* token after the bot mention as the
//! candidate agent name and return the remaining text.
//!
//! Slack renders user mentions as `<@U012345>` in the event payload —
//! never as plain `@Foo`. The text we receive for a channel message like
//! `@RelayBot @Researcher please summarise X` looks like:
//!
//! ```text
//! <@U0BOT>  @Researcher please summarise X
//! ```
//!
//! After stripping everything up to and including the bot mention, we
//! peel off the first plain token (with or without a leading `@`) and
//! treat it as the candidate agent name. The bridge resolves the name
//! against the org's agents case-insensitively and falls back to the
//! default agent on miss.
//!
//! Parser-only — no DB, no async. Agent resolution lives in `bridge.rs`.

use super::types::SlackUserId;

/// Result of mention parsing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MentionParse {
    /// First token after the bot mention; `None` if the message was
    /// just a bare bot mention with no follow-up text.
    pub agent_name: Option<String>,
    /// Everything after the agent name (or after the bot mention, if no
    /// agent name was present). Whitespace-trimmed at both ends.
    pub stripped: String,
}

/// Parse the text portion of an `app_mention` event.
///
/// Behaviour:
/// - Strip the *first* `<@BOT>` substring from `text` (Slack guarantees
///   the bot mention is present in an `app_mention` event).
/// - Trim leading whitespace.
/// - If the next token begins with `@`, treat the rest of that token as
///   the candidate agent name. Otherwise, take the whole first token
///   regardless of prefix — users frequently write `@Foo` without
///   realising Slack doesn't auto-link plain `@names`, and writing it
///   without the `@` is even more common (`/foo` style).
/// - Return the remainder as `stripped`.
#[must_use]
pub fn parse(text: &str, bot: &SlackUserId) -> MentionParse {
    let bot_mention = format!("<@{}>", bot.as_str());
    // Discard everything *before* the bot mention — text preceding the
    // mention is conversational filler ("hey @RelayBot…") that should
    // not be misinterpreted as an agent name candidate.
    let after_bot = text
        .find(&bot_mention)
        .map_or(text, |idx| &text[idx + bot_mention.len()..]);

    let trimmed = after_bot.trim();
    if trimmed.is_empty() {
        return MentionParse {
            agent_name: None,
            stripped: String::new(),
        };
    }

    // Peel the first whitespace-delimited token.
    let mut split = trimmed.splitn(2, char::is_whitespace);
    let first = split.next().unwrap_or("");
    let rest = split.next().unwrap_or("").trim();

    // Strip a leading `@` if present.
    let candidate = first.strip_prefix('@').unwrap_or(first);

    if candidate.is_empty() {
        return MentionParse {
            agent_name: None,
            stripped: rest.to_string(),
        };
    }

    // An agent-name candidate must be a name shape — letters, digits,
    // hyphen, underscore. Anything else (`<`, `>`, punctuation, emoji)
    // means the user just started typing their question; surface no
    // agent name and pass through the whole remaining text.
    let looks_like_name = candidate
        .chars()
        .all(|c| c.is_alphanumeric() || c == '-' || c == '_');
    if !looks_like_name {
        return MentionParse {
            agent_name: None,
            stripped: trimmed.to_string(),
        };
    }

    MentionParse {
        agent_name: Some(candidate.to_string()),
        stripped: rest.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bot() -> SlackUserId {
        SlackUserId::try_from("U0BOT").expect("valid bot id")
    }

    #[test]
    fn bot_mention_with_agent_and_text() {
        let p = parse("<@U0BOT> @researcher summarise this paper", &bot());
        assert_eq!(p.agent_name.as_deref(), Some("researcher"));
        assert_eq!(p.stripped, "summarise this paper");
    }

    #[test]
    fn bot_mention_with_agent_no_text() {
        let p = parse("<@U0BOT> @researcher", &bot());
        assert_eq!(p.agent_name.as_deref(), Some("researcher"));
        assert_eq!(p.stripped, "");
    }

    #[test]
    fn bot_mention_only() {
        let p = parse("<@U0BOT>", &bot());
        assert_eq!(p.agent_name, None);
        assert_eq!(p.stripped, "");
    }

    #[test]
    fn agent_name_without_leading_at() {
        let p = parse("<@U0BOT> researcher hello", &bot());
        assert_eq!(p.agent_name.as_deref(), Some("researcher"));
        assert_eq!(p.stripped, "hello");
    }

    #[test]
    fn agent_name_case_preserved() {
        // The bridge does case-insensitive lookup; we preserve the
        // user's case for diagnostics.
        let p = parse("<@U0BOT> Researcher hello", &bot());
        assert_eq!(p.agent_name.as_deref(), Some("Researcher"));
    }

    #[test]
    fn agent_name_with_hyphen_and_underscore() {
        let p = parse("<@U0BOT> @data-engineer_v2 q", &bot());
        assert_eq!(p.agent_name.as_deref(), Some("data-engineer_v2"));
        assert_eq!(p.stripped, "q");
    }

    #[test]
    fn punctuation_first_token_is_not_an_agent_name() {
        // User wrote a question that starts with punctuation — don't
        // misinterpret as an agent.
        let p = parse("<@U0BOT> ...hello?", &bot());
        assert_eq!(p.agent_name, None);
        assert_eq!(p.stripped, "...hello?");
    }

    #[test]
    fn pre_mention_text_is_discarded() {
        // "hey @RelayBot @critic thoughts?" — pre-mention "hey" is
        // conversational filler and must not be misread as an agent
        // candidate.
        let p = parse("hey <@U0BOT> @critic thoughts?", &bot());
        assert_eq!(p.agent_name.as_deref(), Some("critic"));
        assert_eq!(p.stripped, "thoughts?");
    }

    #[test]
    fn no_bot_mention_falls_through() {
        // The webhook handler shouldn't deliver a payload without a bot
        // mention, but if it does, parse the text as-is.
        let p = parse("@researcher hi", &bot());
        assert_eq!(p.agent_name.as_deref(), Some("researcher"));
        assert_eq!(p.stripped, "hi");
    }

    #[test]
    fn bare_at_with_no_name_is_not_an_agent() {
        let p = parse("<@U0BOT> @ hello", &bot());
        // "@" stripped → empty candidate → no agent, "hello" preserved.
        assert_eq!(p.agent_name, None);
        assert_eq!(p.stripped, "hello");
    }

    #[test]
    fn excess_whitespace_is_trimmed() {
        let p = parse("<@U0BOT>   \n  @researcher   summarise  ", &bot());
        assert_eq!(p.agent_name.as_deref(), Some("researcher"));
        assert_eq!(p.stripped, "summarise");
    }
}
