//! Bounds for the agents subsystem. CLAUDE.md §5: every limit is named, doc-commented,
//! and exported so the operator can audit them in one place.

use std::time::Duration;

/// Maximum length, in bytes, of an agent's display name. Mirrors the
/// `octet_length(name) BETWEEN 1 AND 64` check on the `agents` table.
pub const AGENT_NAME_MAX_LEN: usize = 64;

/// Maximum length, in bytes, of an agent's operator-curated description.
///
/// Sized for ~one sentence (doc/agent_discovery_plan.md §5.4): short
/// enough to be quick to read in a top-K `search_agents` list, large
/// enough to carry useful "what's this role for" signal. Mirrors the
/// `octet_length(description) BETWEEN 1 AND 512` check on the `agents`
/// table.
pub const AGENT_DESCRIPTION_MAX_LEN: usize = 512;

/// Hard cap on the number of agent names rendered inline in the `<agents>` block.
///
/// (doc/agent_discovery_plan.md §8) Below the cap the full alphabetised list
/// renders; above it the block degrades to a one-line "use `search_agents`"
/// notice. Sized to comfortably cover realistic mid-market deployments
/// without forcing every routine hop through semantic search.
pub const MAX_AGENT_NAMES_INLINE: usize = 128;

/// Top-K cap on a single `search_agents` result page.
///
/// (doc/agent_discovery_plan.md §7) Same order of magnitude as
/// [`crate::memory::RECALL_MAX_RESULTS`] so the model's per-turn token
/// budget for a single discovery hop stays bounded.
pub const MAX_SEARCH_AGENT_RESULTS: u8 = 8;

/// Default top-K for `search_agents` when the caller omits `limit`.
pub const DEFAULT_SEARCH_AGENT_RESULTS: u8 = 4;

/// Maximum length, in bytes, of an agent's role-specific system prompt.
///
/// Mirrors the `octet_length(system_prompt) BETWEEN 1 AND 65536` check on the
/// `agents` table. Sized so the assembled `<core> + <role>` string still fits
/// comfortably within typical model context windows.
pub const AGENT_SYSTEM_PROMPT_MAX_LEN: usize = 64 * 1024;

/// Capacity of the per-worker [`crate::agents::AgentPromptCache`].
///
/// Bounds the live working set in worker memory; the `agents` table itself is
/// unbounded (SaaS), and rare tenants whose agent isn't cached pay one DB read
/// per turn.
pub const AGENT_PROMPT_CACHE_CAP: usize = 256;

/// TTL for cached agent prompts. Edits to an agent's `system_prompt` row become
/// visible to live workers within this window — no LISTEN/NOTIFY required.
pub const AGENT_PROMPT_CACHE_TTL: Duration = Duration::from_secs(60);

/// Maximum number of MCP catalog ids that may sit in one agent's
/// `allowed_mcp_tools` map (i.e. distinct keys at the top level).
///
/// Mirrors `crate::mcp::MAX_MCP_SERVERS`: an agent could legitimately be
/// granted every catalog entry the tenant has visible, so a tighter
/// per-agent cap would just create a confusing asymmetry. The schema
/// `CHECK` on `allowed_mcp_tools` enforces the same number on the DB side.
pub const MAX_ALLOWED_MCP_CATALOGS_PER_AGENT: usize = 32;

/// Maximum number of per-catalog tool names an agent may carry in its
/// `allowed_mcp_tools` map's value list.
///
/// Mirrors [`crate::mcp::MAX_TOOLS_PER_SERVER`] — there's no point allowing
/// a per-catalog subset larger than the server itself can expose. The
/// schema `CHECK` on `allowed_mcp_tools` enforces the same number on the
/// DB side.
pub const MAX_ALLOWED_MCP_TOOLS_PER_CATALOG_PER_AGENT: usize = 64;
