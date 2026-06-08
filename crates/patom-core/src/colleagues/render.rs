//! `<colleagues>` roster renderer (Colleagues plan, Stage 6).
//!
//! Supersedes the former agent-only `<agents>` name index with a roster of
//! *every* colleague — humans and agents alike — so the agent perceives human
//! coworkers as addressable peers. Each entry carries its [`ColleagueId`] because the
//! colleague-id addressing introduced by `memory_write` (Stage 5) and
//! `send_message` (Stage 7) needs an id to pass back.
//!
//! The XML tag is `<colleagues>` (renamed from the legacy `<agents>` now that
//! the block names humans too); the prompts and tool descriptions that point
//! the model at it were updated in lock-step.
//!
//! The renderer is a pure, synchronous leaf (§4): names are already resolved on
//! the [`ColleagueRef`]s and the viewer is excluded before formatting. The
//! caller hydrates and caches the roster; this function only formats.

use std::fmt::Write;

use super::limits::MAX_ROSTER_INLINE;
use super::types::{ColleagueId, ColleagueRef};

/// XML-ish envelope tags. Public so tests can assert on wire shape.
pub const ROSTER_TAG_OPEN: &str = "<colleagues>\n";
pub const ROSTER_TAG_CLOSE: &str = "\n</colleagues>";

/// Envelope for the per-session "who you're talking to" line.
pub const SPEAKING_WITH_TAG_OPEN: &str = "<speaking-with>\n";
pub const SPEAKING_WITH_TAG_CLOSE: &str = "\n</speaking-with>";

/// Render the `<speaking-with>` block naming this session's counterpart.
///
/// Sessions are strictly 2-party, so `counterpart` is the single colleague the
/// agent is addressing this turn. Surfacing its id lets the model pass the
/// right `subject` to `memory_write` (and address the right peer) instead of
/// guessing from the roster — which is ambiguous the moment the org has more
/// than one human. The System counterpart of reflection/resolution sessions
/// has no colleague row, so the caller skips this block for it.
#[must_use]
pub fn render_speaking_with(counterpart: &ColleagueRef) -> String {
    let mut out = String::with_capacity(
        SPEAKING_WITH_TAG_OPEN.len()
            + counterpart.display_name.as_str().len()
            + 112
            + SPEAKING_WITH_TAG_CLOSE.len(),
    );
    out.push_str(SPEAKING_WITH_TAG_OPEN);
    let _ = write!(
        &mut out,
        "You are talking with {name} — {kind}, id {id}. Use this id as `subject` \
         when you record what you learn about them with `memory_write`.",
        name = counterpart.display_name.as_str(),
        kind = counterpart.kind.as_str(),
        id = counterpart.id.as_uuid(),
    );
    out.push_str(SPEAKING_WITH_TAG_CLOSE);
    out
}

/// Render the `<colleagues>` roster block for `viewer`.
///
/// `roster` is the org's colleagues (humans + agents), already alpha-sorted by
/// display name from [`crate::colleagues::ColleagueStore::list_for_org`]. The
/// `viewer`'s own colleague is filtered out before formatting — an agent never
/// lists itself.
///
/// Returns `String::new()` when the org has no peers visible to the viewer (the
/// empty envelope is omitted entirely). Above [`MAX_ROSTER_INLINE`] peers the
/// block degrades to a one-line notice pointing the model at `search_agents`.
#[must_use]
pub fn render_roster_block(roster: &[ColleagueRef], viewer: ColleagueId) -> String {
    // Viewer-excluded set — the visible peer roster, not the global one.
    let peers: Vec<&ColleagueRef> = roster.iter().filter(|c| c.id != viewer).collect();

    if peers.is_empty() {
        return String::new();
    }

    if peers.len() > MAX_ROSTER_INLINE {
        let mut out = String::with_capacity(ROSTER_TAG_OPEN.len() + 128 + ROSTER_TAG_CLOSE.len());
        out.push_str(ROSTER_TAG_OPEN);
        let _ = write!(
            &mut out,
            "{n} colleagues available; use `search_agents` to find one.",
            n = peers.len(),
        );
        out.push_str(ROSTER_TAG_CLOSE);
        return out;
    }

    // `- <name> — <kind>, id <uuid>` per peer. The id lets the model address a
    // colleague by id via `send_message` / `memory_write`; the kind tells it
    // which peers execute turns (agents) versus which are humans it can notify.
    let mut out = String::with_capacity(
        ROSTER_TAG_OPEN.len()
            + peers
                .iter()
                .map(|c| c.display_name.as_str().len() + 56)
                .sum::<usize>()
            + ROSTER_TAG_CLOSE.len(),
    );
    out.push_str(ROSTER_TAG_OPEN);
    out.push_str(
        "Your colleagues. Address one with `send_message` (by id), record what \
         you learn about one with `memory_write` (pass its id as `subject`), or \
         look one up with `search_agents`:\n",
    );
    for (i, c) in peers.iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        let _ = write!(
            &mut out,
            "- {name} — {kind}, id {id}",
            name = c.display_name.as_str(),
            kind = c.kind.as_str(),
            id = c.id.as_uuid(),
        );
    }
    out.push_str(ROSTER_TAG_CLOSE);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::colleagues::{ColleagueKind, ColleagueName};

    fn cref(kind: ColleagueKind, name: &str) -> ColleagueRef {
        ColleagueRef {
            id: ColleagueId::new(),
            kind,
            display_name: ColleagueName::try_from(name).expect("valid name"),
        }
    }

    #[test]
    fn empty_org_renders_empty() {
        let block = render_roster_block(&[], ColleagueId::new());
        assert!(block.is_empty());
    }

    #[test]
    fn only_self_renders_empty() {
        let me = cref(ColleagueKind::Agent, "assistant");
        let viewer = me.id;
        let block = render_roster_block(&[me], viewer);
        assert!(block.is_empty(), "self-only excludes the viewer: {block}");
    }

    #[test]
    fn lists_humans_and_agents_excluding_viewer() {
        let me = cref(ColleagueKind::Agent, "assistant");
        let viewer = me.id;
        let human = cref(ColleagueKind::Human, "Tom");
        let agent = cref(ColleagueKind::Agent, "designer");
        let block = render_roster_block(&[me, human.clone(), agent.clone()], viewer);

        assert!(block.contains("Tom"), "human listed: {block}");
        assert!(block.contains("designer"), "agent listed: {block}");
        assert!(!block.contains("assistant"), "viewer excluded: {block}");
        // Both kinds and ids surface so the model can address by id.
        assert!(block.contains("human"), "human kind tag: {block}");
        assert!(block.contains("agent"), "agent kind tag: {block}");
        assert!(
            block.contains(&human.id.as_uuid().to_string()),
            "human id surfaced: {block}"
        );
        assert!(block.starts_with(ROSTER_TAG_OPEN));
        assert!(block.ends_with(ROSTER_TAG_CLOSE));
    }

    #[test]
    fn speaking_with_names_counterpart_with_id() {
        let tom = cref(ColleagueKind::Human, "Tom");
        let block = render_speaking_with(&tom);
        assert!(block.starts_with(SPEAKING_WITH_TAG_OPEN));
        assert!(block.ends_with(SPEAKING_WITH_TAG_CLOSE));
        assert!(block.contains("Tom"), "names the counterpart: {block}");
        assert!(block.contains("human"), "states the kind: {block}");
        assert!(
            block.contains(&tom.id.as_uuid().to_string()),
            "surfaces the id for `subject`: {block}"
        );
    }

    #[test]
    fn degrades_above_cap() {
        let viewer = ColleagueId::new();
        let mut roster = Vec::with_capacity(MAX_ROSTER_INLINE + 1);
        for i in 0..=MAX_ROSTER_INLINE {
            roster.push(cref(ColleagueKind::Human, &format!("person_{i:03}")));
        }
        let block = render_roster_block(&roster, viewer);
        assert!(block.contains("search_agents"), "degrades: {block}");
        assert!(!block.contains("person_000"), "no inline names: {block}");
    }
}
