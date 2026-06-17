//! Render the per-turn `<participants>` prompt block (L1 + L2, issue #183).
//!
//! L1 — *who is in this thread*: the people who have posted, with the thread's
//! raiser flagged. L2 — *who they are*: a one-line profile snippet per person
//! from the shared board. The block is per-turn (it varies with who has spoken),
//! so the assembly layer emits it in the prompt's per-turn tail — never in the
//! org-stable prefix — to preserve prompt-cache hits.

use crate::colleagues::{ColleagueKind, ColleagueName};
use crate::tools::truncate_to_char_boundary;

use super::limits::{MAX_NOTES_PER_PARTICIPANT, MAX_PARTICIPANTS_INLINE, PROFILE_SNIPPET_LEN};
use super::types::ColleagueProfile;

/// Stable tags wrapping the block. `pub` so tests/docs can assert the wire shape.
pub const PARTICIPANTS_TAG_OPEN: &str = "<participants>\n";
pub const PARTICIPANTS_TAG_CLOSE: &str = "\n</participants>";

/// A one-line "who they are" summary for a `<participants>` entry, capped to
/// [`PROFILE_SNIPPET_LEN`] bytes on a char boundary (§1, §5).
///
/// Unlike [`Role`](super::types::Role) and the other write-input newtypes (which
/// *reject* over-length input), a snippet is a *display projection* of
/// already-valid profile fields, so the constructor truncates to fit rather than
/// failing — the same "derived, never rejected" stance as
/// [`ProfileText`](super::types::ProfileText).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParticipantSnippet(String);

impl ParticipantSnippet {
    /// Truncate `text` to the snippet cap on a char boundary.
    #[must_use]
    pub fn new(text: &str) -> Self {
        Self(cap_to_snippet(text))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// One private `collaborator` note about a participant (#193).
///
/// Capped to [`PROFILE_SNIPPET_LEN`] bytes on a char boundary — a display
/// projection of an already-valid `MemoryContent`, truncated to fit rather than
/// rejected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParticipantNote(String);

impl ParticipantNote {
    /// Truncate `text` to the per-note cap on a char boundary.
    #[must_use]
    pub fn new(text: &str) -> Self {
        Self(cap_to_snippet(text))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// The viewer agent's private notes about one participant: at most
/// [`MAX_NOTES_PER_PARTICIPANT`] entries, each a capped [`ParticipantNote`].
///
/// Both bounds are enforced at construction so no caller — now or later — can
/// widen the per-turn prompt tail past the cap (§1, §5). Producers hand notes
/// in priority order; surplus past the cap is dropped.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ParticipantNotes(Vec<ParticipantNote>);

impl ParticipantNotes {
    /// Keep the first [`MAX_NOTES_PER_PARTICIPANT`] notes (callers pass them in
    /// priority order), capping each to the per-note length.
    #[must_use]
    pub fn from_ordered<I, S>(notes: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let kept: Vec<ParticipantNote> = notes
            .into_iter()
            .take(MAX_NOTES_PER_PARTICIPANT)
            .map(|s| ParticipantNote::new(s.as_ref()))
            .collect();
        assert!(
            kept.len() <= MAX_NOTES_PER_PARTICIPANT,
            "invariant: notes per participant exceed cap {MAX_NOTES_PER_PARTICIPANT}"
        );
        Self(kept)
    }

    pub fn iter(&self) -> impl Iterator<Item = &ParticipantNote> {
        self.0.iter()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// Truncate `text` to [`PROFILE_SNIPPET_LEN`] bytes on a char boundary — the one
/// cap shared by [`ParticipantSnippet`] and [`ParticipantNote`].
fn cap_to_snippet(text: &str) -> String {
    let mut s = text.to_owned();
    truncate_to_char_boundary(&mut s, PROFILE_SNIPPET_LEN);
    s
}

/// One already-resolved person in the thread, ready to render.
#[derive(Debug, Clone)]
pub struct ParticipantLine {
    pub name: ColleagueName,
    pub kind: ColleagueKind,
    /// One-line "who they are" summary (profile for humans, none for agents —
    /// an agent's description already shows in `<colleagues>`).
    pub snippet: Option<ParticipantSnippet>,
    /// The viewer agent's own private `collaborator` notes about this person
    /// (#193) — "what *I* learned working with them". Bounded + truncated by the
    /// [`ParticipantNotes`] type itself; empty when the agent holds none.
    pub notes: ParticipantNotes,
    /// True for the colleague who opened the thread ("raised this").
    pub raised_thread: bool,
}

/// Build the one-line snippet for a human's `<participants>` entry.
///
/// Joins the present profile fields (role, expertise, preferences); the
/// [`ParticipantSnippet`] constructor applies the length cap. Returns `None`
/// when the profile carries nothing renderable.
#[must_use]
pub fn profile_snippet(profile: &ColleagueProfile) -> Option<ParticipantSnippet> {
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
    Some(ParticipantSnippet::new(&parts.join("; ")))
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
            out.push_str(snippet.as_str());
        }
        if line.raised_thread {
            out.push_str(" — raised this thread");
        }
        // Private collaborator notes (#193) sit as indented sub-bullets under
        // the person, so each entry reads as "who they are" + "what I learned".
        // The loop is bounded by `ParticipantNotes` itself (≤
        // MAX_NOTES_PER_PARTICIPANT, capped at construction), so no caller can
        // expand the prompt tail past the cap.
        for note in line.notes.iter() {
            out.push_str("\n  • (you noted) ");
            out.push_str(note.as_str());
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
            snippet: snippet.map(ParticipantSnippet::new),
            notes: ParticipantNotes::default(),
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
    fn profile_snippet_joins_present_fields() {
        let p = ColleagueProfile::new(
            ColleagueId::new(),
            Some(Role::try_from("PM").expect("role")),
            Some(Expertise::try_from("billing").expect("exp")),
            Some(Preferences::try_from("async").expect("pref")),
            None,
        );
        assert_eq!(
            profile_snippet(&p).expect("snippet").as_str(),
            "PM; billing; async"
        );
    }

    #[test]
    fn profile_snippet_none_when_empty() {
        let p = ColleagueProfile::new(ColleagueId::new(), None, None, None, None);
        assert!(profile_snippet(&p).is_none());
    }

    #[test]
    fn snippet_and_note_truncate_to_cap_on_boundary() {
        let over = "x".repeat(PROFILE_SNIPPET_LEN + 50);
        assert!(ParticipantSnippet::new(&over).as_str().len() <= PROFILE_SNIPPET_LEN);
        assert!(ParticipantNote::new(&over).as_str().len() <= PROFILE_SNIPPET_LEN);
        // A multi-byte char straddling the cap is never split.
        let multibyte = "é".repeat(PROFILE_SNIPPET_LEN);
        let note = ParticipantNote::new(&multibyte);
        assert!(note.as_str().is_char_boundary(note.as_str().len()));
    }

    #[test]
    fn notes_capped_at_max_per_participant() {
        let many: Vec<String> = (0..MAX_NOTES_PER_PARTICIPANT + 4)
            .map(|i| format!("note {i}"))
            .collect();
        let notes = ParticipantNotes::from_ordered(many);
        assert_eq!(notes.len(), MAX_NOTES_PER_PARTICIPANT);
        // Priority order is preserved: the first N survive the cap.
        assert_eq!(notes.iter().next().expect("first").as_str(), "note 0");
    }

    #[test]
    fn renders_private_notes_as_sub_bullets() {
        let mut pa = line("Pa", ColleagueKind::Human, Some("Product Manager"), true);
        pa.notes = ParticipantNotes::from_ordered([
            "prefers terse async updates",
            "burned by the Q2 estimate",
        ]);
        let block = render_participants_block(&[pa]);
        assert!(
            block.contains("- Pa (human) — Product Manager — raised this thread"),
            "profile line precedes notes: {block}"
        );
        assert!(
            block.contains("\n  • (you noted) prefers terse async updates"),
            "first note as sub-bullet: {block}"
        );
        assert!(
            block.contains("\n  • (you noted) burned by the Q2 estimate"),
            "second note as sub-bullet: {block}"
        );
    }

    #[test]
    fn no_notes_means_no_sub_bullets() {
        let block = render_participants_block(&[line(
            "Mina",
            ColleagueKind::Human,
            Some("Designer"),
            false,
        )]);
        assert!(!block.contains("(you noted)"), "no note bullets: {block}");
    }
}
