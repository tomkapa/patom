//! Per-turn [`Memory`] backed by the agents registry + the agent's
//! memory store (doc/memory.md §1.3).
//!
//! Each call resolves the viewer's role prompt (cached, TTL-bounded by
//! [`crate::agents::AGENT_PROMPT_CACHE_TTL`]) and composes the final
//! `system` field as
//! `<core>...</core>\n[<organization-rule>...</organization-rule>\n][<colleagues>...</colleagues>\n]<role>{prompt}</role>`
//! followed by `<date>`, the per-org `<language>` directive, and the
//! rendered `<memory>...</memory>` section. The role prompt and memory
//! section are cached per session; the language and the per-org rule
//! are cached per agent via [`SharedOrgLanguageResolver`] and
//! [`SharedOrgRuleResolver`] respectively.
//!
//! `<organization-rule>` sits immediately after `<core>` so the most
//! cache-shared prefix (the per-kind core, identical across every org)
//! is followed by the next-most-stable block (the per-org rule, shared
//! across every agent that org runs). When the org has no rule, the tag
//! is omitted entirely — same pattern as `<memory>` when empty.
//!
//! See [`SessionMemoryCache`]'s module doc for the deliberate divergence
//! from doc/memory.md's "frozen for the session's lifetime" wording: we
//! ship a TTL cache today, not session-state storage.

use std::sync::Arc;

use async_trait::async_trait;

use crate::agents::{AgentId, AgentPromptCache, SharedAgentStore};
use crate::auth::{OrgId, OrganizationRule, SharedOrgLanguageResolver, SharedOrgRuleResolver};
use crate::clock::SharedClock;
use crate::colleagues::{
    ColleagueError, ColleagueId, ColleagueKind, ColleagueName, ColleagueRef, ColleagueRosterCache,
    MAX_PARTICIPANTS_INLINE, MAX_ROSTER_INLINE, PROFILE_SNIPPET_LEN, ParticipantLine,
    SharedColleagueStore, SharedProfileStore, SharedThreadDisplayNames, profile_snippet,
    render_participants_block, render_roster_block,
};
use crate::prompts::Prompts;
use crate::runtime::{RequestKind, RequestKindPayload};
use crate::threads::{ChannelRef, SharedThreadStore, ThreadId, ThreadParticipants};
use crate::types::{Participant, ToolName};

use super::loader::MemorySectionLoader;
use super::traits::{Memory, MemoryError};
use super::types::{MemoryHandle, MemoryId};

/// Stable XML-ish tags wrapping each prompt section. Marked `pub` so
/// consumers (e.g. tests, docs) can assert on the wire format if they
/// need to.
pub const CORE_TAG_OPEN: &str = "<core>\n";
pub const CORE_TAG_CLOSE: &str = "\n</core>\n";
/// `<organization-rule>` wraps the per-org rule the admin set via `PATCH /me/org/rule`.
///
/// Placed immediately after `<core>` so the cross-org-shared core stays at
/// the head of the prefix and the per-org rule sits next, giving Anthropic
/// prompt caching a long stable run before any per-agent content. Omitted
/// entirely when the org has no rule configured.
pub const ORG_RULE_TAG_OPEN: &str = "<organization-rule>\n";
pub const ORG_RULE_TAG_CLOSE: &str = "\n</organization-rule>";
pub const ROLE_TAG_OPEN: &str = "<role>\n";
pub const ROLE_TAG_CLOSE: &str = "\n</role>";
pub const DATE_TAG_OPEN: &str = "<date>\n";
pub const DATE_TAG_CLOSE: &str = "\n</date>";
/// `<language>` wraps the per-org language directive.
///
/// Placed between `<date>` and `<memory>` so the daily-churn date stays
/// adjacent to the per-turn memory tail and the language sits with the
/// other per-org stable-for-this-turn fields.
pub const LANGUAGE_TAG_OPEN: &str = "<language>\n";
pub const LANGUAGE_TAG_CLOSE: &str = "\n</language>";

/// `<channels>` lists the channels the agent is a member of (#178), so it can
/// start a thread in one via `send_message` `to: {"channel": <id>}`. Sits with
/// `<colleagues>` in the per-agent stable prefix.
const CHANNELS_TAG_OPEN: &str = "<channels>\n";
const CHANNELS_TAG_CLOSE: &str = "\n</channels>";

/// Render the `<channels>` block from the agent's channel memberships. Empty
/// string when the agent belongs to no channels (no envelope, no blank line).
/// Above [`MAX_ROSTER_INLINE`] channels it degrades to a one-line pointer,
/// mirroring `<colleagues>`.
fn render_channels_block(channels: &[ChannelRef]) -> String {
    use std::fmt::Write;
    if channels.is_empty() {
        return String::new();
    }
    let mut out = String::new();
    out.push_str(CHANNELS_TAG_OPEN);
    if channels.len() > MAX_ROSTER_INLINE {
        let _ = write!(
            out,
            "{n} channels available; address one with `send_message` `to`.",
            n = channels.len(),
        );
        out.push_str(CHANNELS_TAG_CLOSE);
        return out;
    }
    out.push_str(
        "Channels you belong to. Start a new thread in one with `send_message` \
         (`to`: {\"channel\": \"<id>\"}):\n",
    );
    for (i, c) in channels.iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        let _ = write!(
            out,
            "- {name} — channel, id {id}",
            name = c.name.as_str(),
            id = c.id.as_uuid(),
        );
    }
    out.push_str(CHANNELS_TAG_CLOSE);
    out
}

/// `<approval-gated-tools>` lists the tools this agent must get human approval
/// for before calling (#200, locked decision 2: prompt clause + hard gate). The
/// hard gate enforces it regardless; this proactively tells the agent so it
/// reaches for `ask_approval` first instead of hitting the gate.
const GATED_TAG_OPEN: &str = "<approval-gated-tools>\n";
const GATED_TAG_CLOSE: &str = "\n</approval-gated-tools>";

/// Render the `<approval-gated-tools>` block. Empty string when the agent gates
/// no tools (no envelope, no blank line) — so the common case is a zero-cost
/// no-op that leaves the prompt-cache prefix unchanged.
fn render_gated_tools_block(tools: &[ToolName]) -> String {
    use std::fmt::Write;
    if tools.is_empty() {
        return String::new();
    }
    let mut out = String::new();
    out.push_str(GATED_TAG_OPEN);
    out.push_str(
        "These actions require human approval. Before calling one, call \
         `ask_approval` (with `gated_tool` set to its name) and wait for the \
         decision — it will be refused until approved:\n",
    );
    for (i, t) in tools.iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        let _ = write!(out, "- {name}", name = t.as_str());
    }
    out.push_str(GATED_TAG_CLOSE);
    out
}

/// `strftime` pattern for the `<date>` body.
///
/// ISO 8601 date + weekday name + timezone tag — gives the model both
/// machine-parseable and human-friendly anchors for relative-date reasoning
/// ("next Friday", "tomorrow").
pub const DATE_FORMAT: &str = "%Y-%m-%d (%A, UTC)";

/// `"\n"` when `s` is non-empty, else `""` — the separator before an optional
/// prompt section so an absent block leaves no stray blank line.
const fn newline_sep(s: &str) -> &'static str {
    if s.is_empty() { "" } else { "\n" }
}

/// Composite memory backing the per-turn system prompt.
///
/// Assembles `<core>` + `<role>` + `<date>` + `<language>` + `<memory>`
/// from the per-mode core (single-language), a per-agent role string
/// fetched on demand, a per-session composed memory section, and the
/// per-org language directive resolved on every turn.
///
/// `prompt_cache` and `loader` are cheap-clone handles — both hold their
/// own `Arc` state internally, so sharing across subsystems is just a
/// clone. The loader is the single point that builds composed sections;
/// the memory tool layer (`MemoryToolDeps`) takes the same loader so
/// handle resolution and prompt rendering can never diverge.
pub struct AgentMemory {
    agents: SharedAgentStore,
    prompt_cache: AgentPromptCache,
    colleagues: SharedColleagueStore,
    /// Org-shared profile board — feeds the L2 snippets in `<participants>`.
    profiles: SharedProfileStore,
    roster_cache: ColleagueRosterCache,
    /// Per-thread display-name overrides (e.g. Slack handles in a
    /// Slack-rooted thread). Keyed by colleague id; identity is unchanged.
    display_names: SharedThreadDisplayNames,
    loader: MemorySectionLoader,
    prompts: Arc<Prompts>,
    language_resolver: SharedOrgLanguageResolver,
    rule_resolver: SharedOrgRuleResolver,
    /// Source of the agent's channel memberships for the `<channels>` block (#178).
    threads: SharedThreadStore,
    clock: SharedClock,
    /// Per-agent gated-tool config for the `<approval-gated-tools>` block (#200).
    /// `None` in unit tests (block renders empty); the composition root wires the
    /// approval store. Optional so existing `new` callers are unaffected.
    gated_tools: Option<crate::approvals::SharedGatedToolStore>,
}

impl AgentMemory {
    // §4 ceiling on parameter count vs. the alternative — an opaque
    // `AgentMemoryDeps` struct here — would just add ceremony at every
    // call site without making the wiring clearer. Composition root +
    // tests are the only callers; both pass each collaborator by name.
    #[must_use]
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        agents: SharedAgentStore,
        prompt_cache: AgentPromptCache,
        colleagues: SharedColleagueStore,
        profiles: SharedProfileStore,
        roster_cache: ColleagueRosterCache,
        display_names: SharedThreadDisplayNames,
        loader: MemorySectionLoader,
        prompts: Arc<Prompts>,
        language_resolver: SharedOrgLanguageResolver,
        rule_resolver: SharedOrgRuleResolver,
        threads: SharedThreadStore,
        clock: SharedClock,
    ) -> Self {
        Self {
            agents,
            prompt_cache,
            colleagues,
            profiles,
            roster_cache,
            display_names,
            loader,
            prompts,
            language_resolver,
            rule_resolver,
            threads,
            clock,
            gated_tools: None,
        }
    }

    /// Wire the per-agent gated-tool config so the `<approval-gated-tools>` block
    /// renders (#200). Without it, the block is always empty.
    #[must_use]
    pub fn with_gated_tools(mut self, gated_tools: crate::approvals::SharedGatedToolStore) -> Self {
        self.gated_tools = Some(gated_tools);
        self
    }

    /// Resolve a `M-NN` handle the model produced back to the underlying
    /// [`MemoryId`]. Returns `None` if the handle was never minted for this
    /// agent's stable section — typically a hallucinated reference or a row
    /// that has since been forgotten.
    ///
    /// Composes the section on the spot; this is the same path
    /// `system_prompt_for_thread` takes, so the handles a tool resolves match
    /// what the model saw rendered.
    pub async fn resolve_handle(
        &self,
        agent: AgentId,
        kind_payload: &RequestKindPayload,
        handle: MemoryHandle,
    ) -> Result<Option<MemoryId>, MemoryError> {
        self.loader
            .resolve_handle(agent, kind_payload, handle)
            .await
    }

    /// Render the `<colleagues>` colleague-roster block for the viewer.
    ///
    /// Renders the org-wide roster (humans + agents) from the bounded TTL cache,
    /// excluding the viewer itself. Returns a fallible result so the caller can
    /// degrade to an empty block on a directory outage — the roster is an
    /// enrichment, not load-bearing for the turn. `org` is resolved once by the
    /// caller and shared with [`channels_block`](Self::channels_block).
    async fn roster_block(
        &self,
        org: OrgId,
        viewer: ColleagueId,
        overrides: &std::collections::HashMap<ColleagueId, crate::colleagues::ColleagueName>,
    ) -> Result<String, ColleagueError> {
        let roster = self.roster_cache.get_or_load(org, &self.colleagues).await?;
        // §5 saturation signal — no OTel Meter infra yet, so the bound is
        // watched via a structured event the OTel bridge exports.
        tracing::debug!(
            patom.colleagues.roster.size = roster.len(),
            patom.org.id = %org,
            "colleagues.roster.size"
        );
        // The roster cache stays canonical and shared; per-platform labels
        // (e.g. Slack handles) apply per render, keyed by colleague id.
        Ok(render_roster_block(&roster, viewer, overrides))
    }

    /// Render the `<channels>` block: the channels the viewer is a member of, so
    /// it can address one via `send_message` `to: {channel}` (#178). Fallible so
    /// the caller degrades to an empty block on a store outage (enrichment, not
    /// load-bearing). `org` is resolved once by the caller and shared with
    /// [`roster_block`](Self::roster_block).
    async fn channels_block(&self, org: OrgId, viewer: ColleagueId) -> Result<String, MemoryError> {
        let channels = self
            .threads
            .channels_for_colleague(org, viewer)
            .await
            .map_err(|e| MemoryError::Backend(format!("channels_for_colleague: {e}")))?;
        tracing::debug!(
            patom.channels.size = channels.len(),
            patom.org.id = %org,
            "channels.block.size"
        );
        Ok(render_channels_block(&channels))
    }

    /// Render the `<approval-gated-tools>` block for the agent (#200). Empty when
    /// no config store is wired or the agent gates nothing; a store error
    /// degrades to empty (the hard gate still enforces — this block is only the
    /// proactive hint).
    async fn gated_tools_block(&self, org: OrgId, agent: AgentId) -> String {
        let Some(store) = self.gated_tools.as_ref() else {
            return String::new();
        };
        match store.gated_tools_for_agent(org, agent).await {
            Ok(tools) => render_gated_tools_block(&tools),
            Err(e) => {
                tracing::warn!(error = %e, patom.agent.id = %agent, "gated_tools.block.error");
                String::new()
            }
        }
    }

    /// Build the `<participants>` lines for the thread: the raiser + distinct
    /// posters (viewer excluded), each enriched with their shared profile.
    /// Fallible so the public method can degrade the whole block to empty; the
    /// profile lookup degrades independently (names still render without L2).
    async fn try_participants_block(
        &self,
        participants: &ThreadParticipants,
        viewer: ColleagueId,
        overrides: &std::collections::HashMap<ColleagueId, ColleagueName>,
    ) -> Result<String, ColleagueError> {
        // Ordered, deduped, viewer-excluded: creator first, then posters.
        let mut ordered: Vec<ColleagueId> = Vec::new();
        let mut seen = std::collections::HashSet::new();
        seen.insert(viewer);
        if let Some(creator) = participants.creator
            && seen.insert(creator)
        {
            ordered.push(creator);
        }
        for &sender in &participants.senders {
            if ordered.len() >= MAX_PARTICIPANTS_INLINE {
                break;
            }
            if seen.insert(sender) {
                ordered.push(sender);
            }
        }
        if ordered.is_empty() {
            return Ok(String::new());
        }

        // Names + kinds from the org roster (cached, shared with `<colleagues>`).
        let org = self.colleagues.read(viewer).await?.org_id();
        let roster = self.roster_cache.get_or_load(org, &self.colleagues).await?;
        let by_id: std::collections::HashMap<ColleagueId, &ColleagueRef> =
            roster.iter().map(|c| (c.id, c)).collect();

        // L2: profiles for the humans on the list. Degrades independently — a
        // board outage drops snippets, never names.
        let human_ids: Vec<ColleagueId> = ordered
            .iter()
            .copied()
            .filter(|id| {
                by_id
                    .get(id)
                    .is_some_and(|c| c.kind == ColleagueKind::Human)
            })
            .collect();
        let profiles = match self.profiles.get_many(&human_ids).await {
            Ok(map) => map,
            Err(e) => {
                tracing::warn!(error = %e, "participants.profiles.error");
                std::collections::HashMap::new()
            }
        };

        let mut lines: Vec<ParticipantLine> = Vec::with_capacity(ordered.len());
        for id in ordered {
            let Some(colleague) = by_id.get(&id) else {
                continue;
            };
            let name = overrides
                .get(&id)
                .cloned()
                .unwrap_or_else(|| colleague.display_name.clone());
            let snippet = if colleague.kind == ColleagueKind::Human {
                profiles
                    .get(&id)
                    .and_then(|p| profile_snippet(p, PROFILE_SNIPPET_LEN))
            } else {
                None
            };
            lines.push(ParticipantLine {
                name,
                kind: colleague.kind,
                snippet,
                raised_thread: participants.creator == Some(id),
            });
        }

        // §5 saturation signal for the inline list.
        tracing::debug!(
            patom.participants.block.size = lines.len(),
            "participants.block.size"
        );
        Ok(render_participants_block(&lines))
    }
}

impl std::fmt::Debug for AgentMemory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentMemory").finish_non_exhaustive()
    }
}

#[async_trait]
impl Memory for AgentMemory {
    async fn system_prompt_for_thread(
        &self,
        viewer: Participant,
        overrides: &std::collections::HashMap<ColleagueId, crate::colleagues::ColleagueName>,
        kind_payload: &RequestKindPayload,
    ) -> Result<Arc<str>, MemoryError> {
        let agent_id = viewer.agent_id().ok_or_else(|| {
            MemoryError::Backend(
                "system_prompt_for_thread called with non-agent viewer; agent worker only".into(),
            )
        })?;
        let viewer_colleague = viewer.colleague_id().ok_or_else(|| {
            MemoryError::Backend(
                "system_prompt_for_thread viewer has no colleague_id; agent worker only".into(),
            )
        })?;

        let role = self
            .prompt_cache
            .get_or_load(agent_id, &self.agents)
            .await?;
        // Stable layer only — the session-keyed contextual retrieval is not yet
        // rehomed onto the thread feed (degrades empty; enrichment, not
        // load-bearing). The `<colleagues>` roster still renders.
        let memory_section = self.loader.load_stable(agent_id, kind_payload).await?;
        // Resolve the viewer's org once and render the `<colleagues>` and
        // `<channels>` blocks concurrently — both are per-(org, viewer) reads on
        // every turn, independent of each other, and both degrade to empty.
        let (roster, channels, gated) = match self.colleagues.read(viewer_colleague).await {
            Ok(colleague) => {
                let org = colleague.org_id();
                let (roster, channels, gated) = tokio::join!(
                    self.roster_block(org, viewer_colleague, overrides),
                    self.channels_block(org, viewer_colleague),
                    self.gated_tools_block(org, agent_id),
                );
                let roster = roster.unwrap_or_else(|e| {
                    tracing::warn!(error = %e, "colleagues.roster.error");
                    String::new()
                });
                let channels = channels.unwrap_or_else(|e| {
                    tracing::warn!(error = %e, "channels.block.error");
                    String::new()
                });
                (roster, channels, gated)
            }
            Err(e) => {
                tracing::warn!(error = %e, "colleagues.viewer_org.error");
                (String::new(), String::new(), String::new())
            }
        };
        let language = self.language_resolver.language_for_agent(agent_id).await?;
        let directive = self.prompts.set(language).language_directive.clone();
        let org_rule = self.rule_resolver.rule_for_agent(agent_id).await?;

        // No `<speaking-with>` line: a thread feed is multi-party, so there is
        // no single counterpart to name.
        Ok(self.assemble_prompt(
            kind_payload.kind(),
            role.as_str(),
            org_rule.as_ref().map(OrganizationRule::as_str),
            &roster,
            &channels,
            &gated,
            "",
            directive.as_ref(),
            memory_section.text(),
        ))
    }

    async fn display_overrides(
        &self,
        thread: Option<ThreadId>,
    ) -> std::collections::HashMap<ColleagueId, crate::colleagues::ColleagueName> {
        match thread {
            Some(t) => self.display_names.overrides_for_thread(t).await,
            None => std::collections::HashMap::new(),
        }
    }

    async fn participants_block(
        &self,
        participants: &ThreadParticipants,
        viewer: ColleagueId,
        overrides: &std::collections::HashMap<ColleagueId, ColleagueName>,
    ) -> String {
        match self
            .try_participants_block(participants, viewer, overrides)
            .await
        {
            Ok(block) => block,
            Err(e) => {
                tracing::warn!(error = %e, "participants.block.error");
                String::new()
            }
        }
    }

    async fn agent_persona(&self, agent: AgentId) -> Option<Arc<str>> {
        // Same cache the per-turn prompt assembly warms, so this is free on the
        // common path. A lookup failure degrades to no lens (best-effort #202).
        match self.prompt_cache.get_or_load(agent, &self.agents).await {
            Ok(prompt) => Some(prompt.into_arc()),
            Err(e) => {
                tracing::warn!(error = %e, patom.agent.id = %agent, "agent_persona.load.error");
                None
            }
        }
    }
}

impl AgentMemory {
    /// Assemble the final system-prompt string from its already-resolved
    /// pieces. The tag order and cache-prefix layout live in one place.
    /// `speaking_with` is the empty string when there is no single counterpart
    /// to name (always the case on the multi-party thread feed today).
    #[allow(clippy::too_many_arguments)]
    fn assemble_prompt(
        &self,
        kind: RequestKind,
        role_str: &str,
        org_rule: Option<&str>,
        roster: &str,
        channels: &str,
        gated: &str,
        speaking_with: &str,
        directive_str: &str,
        memory_str: &str,
    ) -> Arc<str> {
        let core_arc = self.prompts.cores.for_kind(kind);
        let core = core_arc.as_ref();
        let (rule_open, rule_body, rule_close) =
            org_rule.map_or(("", "", ""), |r| (ORG_RULE_TAG_OPEN, r, ORG_RULE_TAG_CLOSE));
        let rule_sep = newline_sep(rule_open);
        let memory_sep = newline_sep(memory_str);
        let roster_sep = newline_sep(roster);
        // `<channels>` sits with `<colleagues>` in the per-agent stable prefix.
        let channels_sep = newline_sep(channels);
        // `<approval-gated-tools>` is also per-agent stable config, so it joins
        // the cache prefix alongside `<channels>` (#200).
        let gated_sep = newline_sep(gated);
        // Per-turn tail (after `<language>`) — keeping it out of the org-stable
        // prefix preserves prompt-cache hits across the agent's other turns.
        let speaking_with_sep = newline_sep(speaking_with);

        // `<date>` sits between `<role>` and `<memory>` so the daily-churn seam
        // lies between the per-agent stable prefix and the per-turn memory tail.
        // `<language>` follows `<date>` because it is also per-turn (cheap to
        // re-render) and rotates with the org's setting rather than the agent's.
        let now_utc: chrono::DateTime<chrono::Utc> = self.clock.now_wall().into();
        let date_str = now_utc.format(DATE_FORMAT).to_string();
        let date_sep = "\n";
        let lang_sep = "\n";

        let mut out = String::with_capacity(
            CORE_TAG_OPEN.len()
                + core.len()
                + CORE_TAG_CLOSE.len()
                + rule_open.len()
                + rule_body.len()
                + rule_close.len()
                + rule_sep.len()
                + roster.len()
                + roster_sep.len()
                + channels.len()
                + channels_sep.len()
                + gated.len()
                + gated_sep.len()
                + ROLE_TAG_OPEN.len()
                + role_str.len()
                + ROLE_TAG_CLOSE.len()
                + date_sep.len()
                + DATE_TAG_OPEN.len()
                + date_str.len()
                + DATE_TAG_CLOSE.len()
                + lang_sep.len()
                + LANGUAGE_TAG_OPEN.len()
                + directive_str.len()
                + LANGUAGE_TAG_CLOSE.len()
                + speaking_with_sep.len()
                + speaking_with.len()
                + memory_sep.len()
                + memory_str.len(),
        );
        out.push_str(CORE_TAG_OPEN);
        out.push_str(core);
        out.push_str(CORE_TAG_CLOSE);
        // `<organization-rule>` between `</core>` and `<colleagues>` — see
        // module doc for the cache-prefix rationale. Empty strings when
        // the org has no rule, so no separator slips through.
        out.push_str(rule_open);
        out.push_str(rule_body);
        out.push_str(rule_close);
        out.push_str(rule_sep);
        out.push_str(roster);
        out.push_str(roster_sep);
        out.push_str(channels);
        out.push_str(channels_sep);
        out.push_str(gated);
        out.push_str(gated_sep);
        out.push_str(ROLE_TAG_OPEN);
        out.push_str(role_str);
        out.push_str(ROLE_TAG_CLOSE);
        out.push_str(date_sep);
        out.push_str(DATE_TAG_OPEN);
        out.push_str(&date_str);
        out.push_str(DATE_TAG_CLOSE);
        out.push_str(lang_sep);
        out.push_str(LANGUAGE_TAG_OPEN);
        out.push_str(directive_str);
        out.push_str(LANGUAGE_TAG_CLOSE);
        out.push_str(speaking_with_sep);
        out.push_str(speaking_with);
        out.push_str(memory_sep);
        out.push_str(memory_str);

        Arc::from(out)
    }
}

#[cfg(test)]
mod tests {
    use super::{GATED_TAG_OPEN, render_gated_tools_block};
    use crate::types::ToolName;

    #[test]
    fn gated_block_is_empty_when_no_tools() {
        assert_eq!(render_gated_tools_block(&[]), "");
    }

    #[test]
    fn gated_block_lists_each_tool_and_points_at_ask_approval() {
        let tools = [
            ToolName::try_from("refund_customer").expect("name"),
            ToolName::try_from("merge_pr").expect("name"),
        ];
        let block = render_gated_tools_block(&tools);
        assert!(block.starts_with(GATED_TAG_OPEN));
        assert!(block.contains("ask_approval"));
        assert!(block.contains("- refund_customer"));
        assert!(block.contains("- merge_pr"));
        assert!(block.ends_with("</approval-gated-tools>"));
    }
}
