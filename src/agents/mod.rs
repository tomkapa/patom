//! Agents registry — multi-agent support.
//!
//! Each row in the `agents` table is one persona: a display name plus a
//! role-specific system prompt. The composition root seeds a single default
//! agent at startup ([`PgAgentStore::seed_default`]); session-create binds a
//! session to either an explicitly-supplied `agent_id` or the default. Every
//! turn loads the agent's prompt through [`AgentPromptCache`] (60s TTL) and
//! assembles the final `system` field as
//! `<core>...</core>\n<role>{prompt}</role>` — see
//! [`crate::memory::AgentMemory`] for the assembly seam.
//!
//! Distinct from [`crate::agent_core`]: that module owns the runtime orchestrator
//! (`Agent`, the chat loop). This one owns the *registry* of agent definitions.

mod cache;
mod error;
mod limits;
mod pg_store;
mod registry;
mod render;
mod store;
mod types;

pub use cache::{AgentNamesCache, AgentPromptCache};
pub use error::AgentStoreError;
pub use limits::{
    AGENT_DESCRIPTION_MAX_LEN, AGENT_NAME_MAX_LEN, AGENT_PROMPT_CACHE_CAP, AGENT_PROMPT_CACHE_TTL,
    AGENT_SYSTEM_PROMPT_MAX_LEN, DEFAULT_SEARCH_AGENT_RESULTS, MAX_AGENT_NAMES_INLINE,
    MAX_ALLOWED_MCP_CATALOGS_PER_AGENT, MAX_ALLOWED_MCP_TOOLS_PER_CATALOG_PER_AGENT,
    MAX_SEARCH_AGENT_RESULTS,
};
pub use pg_store::PgAgentStore;
pub use registry::{AgentFactory, Agents, AgentsError, CachedAgents, SharedAgents};
pub use render::{AGENTS_TAG_CLOSE, AGENTS_TAG_OPEN, render_agents_block};
pub use store::{AgentStore, AgentUpdate, NewAgent, SharedAgentStore};
pub use types::{
    AgentCard, AgentDescription, AgentId, AgentName, AgentRecord, AgentSystemPrompt,
    AllowedMcpTools, DefaultAgentSeed, ToolScope,
};
