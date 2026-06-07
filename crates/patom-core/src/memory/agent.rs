//! Per-turn [`Memory`] backed by the agents registry + the agent's
//! memory store (doc/memory.md §1.3).
//!
//! Each call resolves the viewer's role prompt (cached, TTL-bounded by
//! [`crate::agents::AGENT_PROMPT_CACHE_TTL`]) and composes the final
//! `system` field as
//! `<core>...</core>\n[<organization-rule>...</organization-rule>\n][<agents>...</agents>\n]<role>{prompt}</role>`
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
use crate::auth::{SharedOrgLanguageResolver, SharedOrgRuleResolver};
use crate::clock::SharedClock;
use crate::colleagues::{
    ColleagueError, ColleagueId, ColleagueRosterCache, SharedColleagueStore, render_roster_block,
};
use crate::prompts::Prompts;
use crate::runtime::RequestKindPayload;
use crate::session::SessionId;
use crate::types::Participant;

use super::composer::MemorySection;
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

/// `strftime` pattern for the `<date>` body.
///
/// ISO 8601 date + weekday name + timezone tag — gives the model both
/// machine-parseable and human-friendly anchors for relative-date reasoning
/// ("next Friday", "tomorrow").
pub const DATE_FORMAT: &str = "%Y-%m-%d (%A, UTC)";

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
    roster_cache: ColleagueRosterCache,
    loader: MemorySectionLoader,
    prompts: Arc<Prompts>,
    language_resolver: SharedOrgLanguageResolver,
    rule_resolver: SharedOrgRuleResolver,
    clock: SharedClock,
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
        roster_cache: ColleagueRosterCache,
        loader: MemorySectionLoader,
        prompts: Arc<Prompts>,
        language_resolver: SharedOrgLanguageResolver,
        rule_resolver: SharedOrgRuleResolver,
        clock: SharedClock,
    ) -> Self {
        Self {
            agents,
            prompt_cache,
            colleagues,
            roster_cache,
            loader,
            prompts,
            language_resolver,
            rule_resolver,
            clock,
        }
    }

    /// Resolve a `M-NN` handle the model produced inside `(session,
    /// agent)` back to the underlying [`MemoryId`]. Returns `None` if
    /// the handle was never minted for this session — typically a
    /// hallucinated reference or a session whose composition has been
    /// evicted from the cache.
    ///
    /// Composes the section on the spot if the cache misses; this is
    /// the same path `system_prompt` takes, so resolving against a
    /// session that just rolled past TTL is a single cache reload, not
    /// an error.
    pub async fn resolve_handle(
        &self,
        session: SessionId,
        agent: AgentId,
        kind_payload: &RequestKindPayload,
        handle: MemoryHandle,
    ) -> Result<Option<MemoryId>, MemoryError> {
        self.loader
            .resolve_handle(session, agent, kind_payload, handle)
            .await
    }

    async fn composed_section(
        &self,
        session: SessionId,
        agent: AgentId,
        kind_payload: &RequestKindPayload,
    ) -> Result<Arc<MemorySection>, MemoryError> {
        self.loader.load(session, agent, kind_payload).await
    }

    /// Render the `<agents>` colleague-roster block for the viewer.
    ///
    /// Resolves the viewer's org from its colleague row, then renders the
    /// org-wide roster (humans + agents) from the bounded TTL cache, excluding
    /// the viewer itself. Returns a fallible result so the caller can degrade
    /// to an empty block on a directory outage — the roster is an enrichment,
    /// not load-bearing for the turn.
    async fn roster_block(&self, viewer: ColleagueId) -> Result<String, ColleagueError> {
        let org = self.colleagues.read(viewer).await?.org_id();
        let roster = self.roster_cache.get_or_load(org, &self.colleagues).await?;
        // §5 saturation signal — no OTel Meter infra yet, so the bound is
        // watched via a structured event the OTel bridge exports.
        tracing::debug!(
            patom.colleagues.roster.size = roster.len(),
            patom.org.id = %org,
            "colleagues.roster.size"
        );
        Ok(render_roster_block(&roster, viewer))
    }
}

impl std::fmt::Debug for AgentMemory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentMemory").finish_non_exhaustive()
    }
}

#[async_trait]
impl Memory for AgentMemory {
    async fn system_prompt(
        &self,
        session: SessionId,
        viewer: Participant,
        kind_payload: &RequestKindPayload,
    ) -> Result<Arc<str>, MemoryError> {
        // Workers only run for agent receivers; a Human viewer is a wiring bug.
        let agent_id = viewer.agent_id().ok_or_else(|| {
            MemoryError::Backend("system_prompt called with Human viewer; agent worker only".into())
        })?;
        let role = self
            .prompt_cache
            .get_or_load(agent_id, &self.agents)
            .await?;
        let memory_section = self
            .composed_section(session, agent_id, kind_payload)
            .await?;

        // `<agents>` colleague roster (Colleagues plan, Stage 6). Lists every
        // colleague in the viewer's org — humans and agents alike — so the
        // agent perceives human coworkers as addressable peers. Cached per org
        // with the same TTL as `AgentPromptCache` so a membership change or
        // rename propagates within one liveness window. Self-only and empty
        // orgs yield an empty string; the renderer omits the envelope. A
        // directory outage degrades to an empty block — the roster enriches the
        // turn but is not load-bearing.
        let viewer_colleague = viewer.colleague_id().ok_or_else(|| {
            MemoryError::Backend(
                "system_prompt viewer has no colleague_id; agent worker only".into(),
            )
        })?;
        let agents_block = match self.roster_block(viewer_colleague).await {
            Ok(block) => block,
            Err(e) => {
                tracing::warn!(error = %e, "colleagues.roster.error");
                String::new()
            }
        };

        // Per-org language. Cached behind the resolver so consecutive
        // turns for the same agent stay one mutex round-trip away from
        // the right directive. A switch propagates via the PATCH
        // /me/org/language handler invalidating the cache.
        let language = self.language_resolver.language_for_agent(agent_id).await?;
        let directive = self.prompts.set(language).language_directive.clone();

        // Per-org `<organization-rule>`. Cached behind its own resolver
        // for the same hot-path reason as the language. `None` is the
        // "no rule configured" sentinel — the tag is omitted entirely
        // so empty configs don't waste prompt budget. The resolver
        // error type intentionally mirrors `LanguageResolverError` and
        // surfaces here as `MemoryError::Backend` via the same
        // conversion path the language uses.
        let org_rule = self.rule_resolver.rule_for_agent(agent_id).await?;
        let (rule_open, rule_body, rule_close) = org_rule.as_ref().map_or(("", "", ""), |r| {
            (ORG_RULE_TAG_OPEN, r.as_str(), ORG_RULE_TAG_CLOSE)
        });
        let rule_sep = if rule_open.is_empty() { "" } else { "\n" };

        let core_arc = self.prompts.cores.for_kind(kind_payload.kind());
        let core = core_arc.as_ref();
        let role_str = role.as_str();
        let memory_str = memory_section.text();
        let memory_sep = if memory_str.is_empty() { "" } else { "\n" };
        let agents_sep = if agents_block.is_empty() { "" } else { "\n" };

        // `<date>` sits between `<role>` and `<memory>` so the daily-churn seam
        // lies between the per-agent stable prefix and the per-turn memory tail.
        // `<language>` follows `<date>` because it is also per-turn (cheap to
        // re-render) and rotates with the org's setting rather than the agent's.
        let now_utc: chrono::DateTime<chrono::Utc> = self.clock.now_wall().into();
        let date_str = now_utc.format(DATE_FORMAT).to_string();
        let date_sep = "\n";
        let lang_sep = "\n";
        let directive_str = directive.as_ref();

        let mut out = String::with_capacity(
            CORE_TAG_OPEN.len()
                + core.len()
                + CORE_TAG_CLOSE.len()
                + rule_open.len()
                + rule_body.len()
                + rule_close.len()
                + rule_sep.len()
                + agents_block.len()
                + agents_sep.len()
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
                + memory_sep.len()
                + memory_str.len(),
        );
        out.push_str(CORE_TAG_OPEN);
        out.push_str(core);
        out.push_str(CORE_TAG_CLOSE);
        // `<organization-rule>` between `</core>` and `<agents>` — see
        // module doc for the cache-prefix rationale. Empty strings when
        // the org has no rule, so no separator slips through.
        out.push_str(rule_open);
        out.push_str(rule_body);
        out.push_str(rule_close);
        out.push_str(rule_sep);
        out.push_str(&agents_block);
        out.push_str(agents_sep);
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
        out.push_str(memory_sep);
        out.push_str(memory_str);

        Ok(Arc::from(out))
    }
}
