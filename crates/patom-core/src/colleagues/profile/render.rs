//! Render the per-turn `<participants>` prompt block (L1 + L2, issue #183).
//!
//! L1 — *who is in this thread*: the people who have posted, with the thread's
//! raiser flagged. L2 — *who they are*: a one-line profile snippet per person
//! from the shared board. The block is per-turn (it varies with who has spoken),
//! so the assembly layer emits it in the prompt's per-turn tail — never in the
//! org-stable prefix — to preserve prompt-cache hits.

use crate::colleagues::{ColleagueKind, ColleagueName};

use super::limits::MAX_PARTICIPANTS_INLINE;
use super::types::ColleagueProfile;

/// Stable tags wrapping the block. `pub` so tests/docs can assert the wire shape.
pub const PARTICIPANTS_TAG_OPEN: &str = "<participants>\n";
pub const PARTICIPANTS_TAG_CLOSE: &str = "\n</participants>";

/// One already-resolved person in the thread, ready to render.
#[derive(Debug, Clone)]
pub struct ParticipantLine {
    pub name: ColleagueName,
    pub kind: ColleagueKind,
    /// One-line "who they are" summary (profile for humans, none for agents —
    /// an agent's description already shows in `<colleagues>`).
    pub snippet: Option<String>,
    /// True for the colleague who opened the thread ("raised this").
    pub raised_thread: bool,
}

/// Build the one-line snippet for a human's `<participants>` entry.
///
/// Joins the present profile fields (role, expertise, preferences) and caps the
/// length. Returns `None` when the profile carries nothing renderable.
#[must_use]
pub fn profile_snippet(profile: &ColleagueProfile, max: usize) -> Option<String> {
    let mut parts: Vec<&str> = Vec::with_capacity(3);
    if let Some(role) = profile.role() {
        parts.push(role.as_str());
    }
    if let Some(expertise) = profile.expertise() {
        parts.push(expertise.as_str());
    }
    if let Some(preferences) = profile.preferences() {
        parts.push(preferences.as_str());
    }
    if parts.is_empty() {
        return None;
    }
    let mut joined = parts.join("; ");
    if joined.len() > max {
        // Truncate on a char boundary at or below `max`.
        let mut end = max;
        while end > 0 && !joined.is_char_boundary(end) {
            end -= 1;
        }
        joined.truncate(end);
    }
    Some(joined)
}

/// Render the `<participants>` block from already-resolved lines.
///
/// Returns the empty string when there are no participants, so the caller emits
/// nothing (no stray tag). The list is capped at [`MAX_PARTICIPANTS_INLINE`].
#[must_use]
pub fn render_participants_block(lines: &[ParticipantLine]) -> String {
    if lines.is_empty() {
        return String::new();
    }

    let raiser = lines
        .iter()
        .find(|l| l.raised_thread)
        .map(|l| l.name.as_str());

    let mut out = String::from(PARTICIPANTS_TAG_OPEN);
    match raiser {
        Some(name) => {
            out.push_str("People in this conversation. This thread was raised by ");
            out.push_str(name);
            out.push('.');
        }
        None => out.push_str("People in this conversation."),
    }

    for line in lines.iter().take(MAX_PARTICIPANTS_INLINE) {
        out.push_str("\n- ");
        out.push_str(line.name.as_str());
        out.push_str(" (");
        out.push_str(line.kind.as_str());
        out.push(')');
        if let Some(snippet) = &line.snippet {
            out.push_str(" — ");
            out.push_str(snippet);
        }
        if line.raised_thread {
            out.push_str(" — raised this thread");
        }
    }

    out.push_str(PARTICIPANTS_TAG_CLOSE);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::colleagues::{ColleagueId, Expertise, Preferences, Role};

    fn line(
        name: &str,
        kind: ColleagueKind,
        snippet: Option<&str>,
        raised: bool,
    ) -> ParticipantLine {
        ParticipantLine {
            name: ColleagueName::try_from(name).expect("name"),
            kind,
            snippet: snippet.map(ToOwned::to_owned),
            raised_thread: raised,
        }
    }

    #[test]
    fn empty_renders_nothing() {
        assert_eq!(render_participants_block(&[]), "");
    }

    #[test]
    fn names_raiser_and_lists_people() {
        let lines = vec![
            line("Pa", ColleagueKind::Human, Some("Product Manager"), true),
            line("Scout", ColleagueKind::Agent, None, false),
        ];
        let block = render_participants_block(&lines);
        assert!(block.starts_with(PARTICIPANTS_TAG_OPEN));
        assert!(block.ends_with(PARTICIPANTS_TAG_CLOSE));
        assert!(block.contains("raised by Pa."));
        assert!(block.contains("- Pa (human) — Product Manager — raised this thread"));
        assert!(block.contains("- Scout (agent)"));
        // Agent with no snippet shows name + kind only.
        assert!(!block.contains("Scout (agent) —"));
    }

    #[test]
    fn no_raiser_uses_neutral_header() {
        let lines = vec![line("Mina", ColleagueKind::Human, None, false)];
        let block = render_participants_block(&lines);
        assert!(block.contains("People in this conversation."));
        assert!(!block.contains("raised by"));
    }

    #[test]
    fn caps_inline_list() {
        let lines: Vec<ParticipantLine> = (0..MAX_PARTICIPANTS_INLINE + 5)
            .map(|i| line(&format!("p{i}"), ColleagueKind::Human, None, false))
            .collect();
        let block = render_participants_block(&lines);
        let count = block.matches("\n- ").count();
        assert_eq!(count, MAX_PARTICIPANTS_INLINE);
    }

    #[test]
    fn profile_snippet_joins_and_truncates() {
        let p = ColleagueProfile::new(
            ColleagueId::new(),
            Some(Role::try_from("PM").expect("role")),
            Some(Expertise::try_from("billing").expect("exp")),
            Some(Preferences::try_from("async").expect("pref")),
            None,
        );
        assert_eq!(
            profile_snippet(&p, 160).expect("snippet"),
            "PM; billing; async"
        );
        // Truncates on a boundary at or below max.
        let short = profile_snippet(&p, 5).expect("snippet");
        assert!(short.len() <= 5);
    }

    #[test]
    fn profile_snippet_none_when_empty() {
        let p = ColleagueProfile::new(ColleagueId::new(), None, None, None, None);
        assert!(profile_snippet(&p, 160).is_none());
    }
}
