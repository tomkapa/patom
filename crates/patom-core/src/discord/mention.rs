//! Pure Discord mention rendering — inbound `<@id>` → `@Name`, outbound
//! `@Name` → `<@id>`.
//!
//! Discord renders a user mention as `<@snowflake>` (or `<@!snowflake>` for a
//! nickname mention). Inbound, we rewrite those to readable `@Name` so the agent
//! reads who is referenced (mirroring the web feed). Outbound, we rewrite an
//! agent's plain `@Name` into a real `<@snowflake>` ping — and return the ids we
//! pinged, so the poster can list exactly those in `allowed_mentions` (the
//! structural defense against an accidental `@everyone`). Simpler than Lark's
//! `<at>` markup: a snowflake is global, so there is no app-scoped-id problem.
//!
//! Parser-only — no DB, no async, no I/O.

use super::types::DiscordUserId;

/// Rewrite a message's `<@id>` / `<@!id>` markers to readable `@Name`.
///
/// Uses the `(id, display_name)` pairs from the event's `mentions` array. An id
/// with no name pair (or a role/`@everyone` marker) is left verbatim.
#[must_use]
pub fn render_inbound(content: &str, mentions: &[(DiscordUserId, String)]) -> String {
    if mentions.is_empty() || !content.contains("<@") {
        return content.to_owned();
    }
    let mut out = content.to_owned();
    for (id, name) in mentions {
        let readable = format!("@{name}");
        out = out.replace(&format!("<@{}>", id.as_str()), &readable);
        out = out.replace(&format!("<@!{}>", id.as_str()), &readable);
    }
    out
}

/// Render an outbound user mention: `<@snowflake>`.
#[must_use]
pub fn render_mention(id: &DiscordUserId) -> String {
    format!("<@{}>", id.as_str())
}

/// Rewrite inline `@Name` mentions in an outbound reply into `<@snowflake>` pings
/// and return the snowflakes pinged (for `allowed_mentions.users`).
///
/// `handles` is `(colleague_display_name, snowflake)` for the org's Discord
/// humans. Mirrors the web UI's `findNamedMentions`: matches `@Name` only at a
/// word boundary (start-of-text or after whitespace — so `a@b.com` is not a
/// mention), longest name first (so `Tom Tran` beats `Tom`), and only when the
/// char after the name is a non-name boundary. A name with no Discord handle is
/// left as plain `@Name` text.
#[must_use]
pub fn render_outbound(
    content: &str,
    handles: &[(String, DiscordUserId)],
) -> (String, Vec<DiscordUserId>) {
    if handles.is_empty() {
        return (content.to_owned(), Vec::new());
    }
    let mut sorted: Vec<&(String, DiscordUserId)> =
        handles.iter().filter(|h| !h.0.is_empty()).collect();
    // Longest name first, so "Tom Tran" wins over "Tom".
    sorted.sort_by_key(|h| std::cmp::Reverse(h.0.len()));

    let mut out = String::with_capacity(content.len());
    let mut pinged: Vec<DiscordUserId> = Vec::new();
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
                out.push_str(&render_mention(&h.1));
                if !pinged.contains(&h.1) {
                    pinged.push(h.1.clone());
                }
                rest = &after_at[h.0.len()..];
                prev_is_boundary = false;
                continue;
            }
        }
        out.push(ch);
        prev_is_boundary = ch.is_whitespace();
        rest = &rest[ch.len_utf8()..];
    }
    (out, pinged)
}

/// A char that can appear inside a mentionable name run (`[A-Za-z0-9_-]`); the
/// name match requires a non-name char (or end) immediately after.
fn is_name_char(c: char) -> bool {
    c.is_alphanumeric() || c == '-' || c == '_'
}

/// Whether `text` already contains an inline `@name` mention at a word boundary
/// (used to dedup an addressed-to marker against the inline render).
#[must_use]
pub fn already_names(text: &str, name: &str) -> bool {
    let needle = format!("@{name}");
    for (pos, _) in text.match_indices(&needle) {
        let prev_is_boundary = text[..pos]
            .chars()
            .next_back()
            .is_none_or(char::is_whitespace);
        let after = &text[pos + needle.len()..];
        let next_is_boundary = after.chars().next().is_none_or(|c| !is_name_char(c));
        if prev_is_boundary && next_is_boundary {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(s: &str) -> DiscordUserId {
        DiscordUserId::try_from(s).expect("valid snowflake")
    }

    #[test]
    fn render_inbound_rewrites_user_and_nick_markers() {
        let mentions = vec![
            (id("111"), "Alice".to_owned()),
            (id("222"), "Bob".to_owned()),
        ];
        let out = render_inbound("hi <@111> and <@!222>, ship it", &mentions);
        assert_eq!(out, "hi @Alice and @Bob, ship it");
    }

    #[test]
    fn render_inbound_leaves_unknown_ids_and_roles() {
        let mentions = vec![(id("111"), "Alice".to_owned())];
        // A role mention `<@&...>` and an unknown user id are untouched.
        let out = render_inbound("<@111> ping <@&999> and <@333>", &mentions);
        assert_eq!(out, "@Alice ping <@&999> and <@333>");
    }

    #[test]
    fn render_inbound_passthrough_without_markers() {
        assert_eq!(
            render_inbound("no mentions here", &[(id("1"), "X".to_owned())]),
            "no mentions here"
        );
    }

    #[test]
    fn render_outbound_pings_known_names_longest_first() {
        let handles = vec![
            ("Tom".to_owned(), id("100")),
            ("Tom Tran".to_owned(), id("200")),
        ];
        let (out, pinged) =
            render_outbound("hi @Tom Tran and @Tom, also a@b.com and @Unknown", &handles);
        assert_eq!(out, "hi <@200> and <@100>, also a@b.com and @Unknown");
        assert_eq!(pinged, vec![id("200"), id("100")]);
    }

    #[test]
    fn render_outbound_only_at_word_boundary() {
        let handles = vec![("Tom".to_owned(), id("100"))];
        let (out, pinged) = render_outbound("email me@Tom and @Tomas", &handles);
        // me@Tom is not a boundary; @Tomas is a longer run → neither pings.
        assert_eq!(out, "email me@Tom and @Tomas");
        assert!(pinged.is_empty());
    }

    #[test]
    fn render_outbound_passthrough_without_handles() {
        let (out, pinged) = render_outbound("@Tom hi", &[]);
        assert_eq!(out, "@Tom hi");
        assert!(pinged.is_empty());
    }

    #[test]
    fn render_outbound_dedups_repeated_pings() {
        let handles = vec![("Tom".to_owned(), id("100"))];
        let (_, pinged) = render_outbound("@Tom then @Tom again", &handles);
        assert_eq!(pinged, vec![id("100")], "the same id is listed once");
    }

    #[test]
    fn already_names_respects_boundaries() {
        assert!(already_names("@Recruiter hi", "Recruiter"));
        assert!(already_names("hi @Recruiter", "Recruiter"));
        assert!(!already_names("@Recruiters meeting", "Recruiter"));
        assert!(!already_names("a@Recruiter", "Recruiter"));
        assert!(!already_names("hello there", "Recruiter"));
    }
}
