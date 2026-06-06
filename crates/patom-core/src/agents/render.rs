//! `<agents>` name index renderer (doc/agent_discovery_plan.md §8).
//!
//! Flat, alphabetised list of every agent name in the deployment, with the
//! caller excluded. Sits between `<core>` and `<role>` in the assembled
//! system prompt, mirroring the position of `<memory>`.
//!
//! The renderer is intentionally tiny: name-only, no descriptions, no ids.
//! Descriptions are a fetched view (via `search_agents`), consistent with
//! the deferred-tools analogy in the design doc §9.2.

use std::fmt::Write;

use super::limits::MAX_AGENT_NAMES_INLINE;
use super::types::{AgentId, AgentName};

/// XML-ish envelope tags. Public so tests can assert on wire shape.
pub const AGENTS_TAG_OPEN: &str = "<agents>\n";
pub const AGENTS_TAG_CLOSE: &str = "\n</agents>";

/// Render the `<agents>` block for `viewer`.
///
/// Inputs are `(id, name)` pairs sourced from the agents store (already
/// alphabetised by `lower(name)` from `AgentStore::list_names`). The
/// caller (`viewer`) is filtered out before formatting (§9.4 of the
/// design — three surfaces exclude the caller consistently).
///
/// Returns `String::new()` when the deployment has zero peers visible to
/// the caller — the empty `<agents></agents>` envelope is omitted entirely
/// (§8). Above `MAX_AGENT_NAMES_INLINE` peers the block degrades to a
/// one-line notice telling the model to use `search_agents`.
#[must_use]
pub fn render_agents_block(all: &[(AgentId, AgentName)], viewer: AgentId) -> String {
    // Caller-excluded count: this is the visible peer set, not the global one.
    let peers: Vec<&AgentName> = all
        .iter()
        .filter(|(id, _)| *id != viewer)
        .map(|(_, name)| name)
        .collect();

    if peers.is_empty() {
        return String::new();
    }

    if peers.len() > MAX_AGENT_NAMES_INLINE {
        let mut out = String::with_capacity(AGENTS_TAG_OPEN.len() + 128 + AGENTS_TAG_CLOSE.len());
        out.push_str(AGENTS_TAG_OPEN);
        let _ = write!(
            &mut out,
            "{n} agents available; use `search_agents` to find one.",
            n = peers.len(),
        );
        out.push_str(AGENTS_TAG_CLOSE);
        return out;
    }

    let mut out = String::with_capacity(
        AGENTS_TAG_OPEN.len()
            + peers.iter().map(|n| n.as_str().len() + 2).sum::<usize>()
            + AGENTS_TAG_CLOSE.len(),
    );
    out.push_str(AGENTS_TAG_OPEN);
    out.push_str(
        "Other agents you can address by name with `send_message` or look up \
         with `search_agents`:\n",
    );
    for (i, name) in peers.iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        out.push_str(name.as_str());
    }
    out.push_str(AGENTS_TAG_CLOSE);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn name(s: &str) -> AgentName {
        AgentName::try_from(s).expect("valid")
    }

    #[test]
    fn empty_deployment_renders_empty() {
        let id = AgentId::new();
        let block = render_agents_block(&[], id);
        assert!(block.is_empty());
    }

    #[test]
    fn only_self_renders_empty() {
        let viewer = AgentId::new();
        let block = render_agents_block(&[(viewer, name("assistant"))], viewer);
        assert!(block.is_empty(), "self-only excludes caller: {block}");
    }

    #[test]
    fn lists_peers_excluding_caller() {
        let viewer = AgentId::new();
        let other = AgentId::new();
        let third = AgentId::new();
        let all = vec![
            (viewer, name("assistant")),
            (other, name("designer")),
            (third, name("translator")),
        ];
        let block = render_agents_block(&all, viewer);
        assert!(block.contains("designer"));
        assert!(block.contains("translator"));
        assert!(!block.contains("assistant"));
        assert!(block.starts_with(AGENTS_TAG_OPEN));
        assert!(block.ends_with(AGENTS_TAG_CLOSE));
    }

    #[test]
    fn degrades_above_cap() {
        let viewer = AgentId::new();
        let mut all = Vec::with_capacity(MAX_AGENT_NAMES_INLINE + 2);
        all.push((viewer, name("assistant")));
        for i in 0..=MAX_AGENT_NAMES_INLINE {
            all.push((AgentId::new(), name(&format!("agent_{i:03}"))));
        }
        let block = render_agents_block(&all, viewer);
        assert!(block.contains("search_agents"));
        assert!(!block.contains("agent_000"));
    }
}
