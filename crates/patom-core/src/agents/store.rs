//! Storage trait + cheap-clone handle for the agents registry.

use std::fmt;
use std::sync::Arc;

use async_trait::async_trait;

use crate::auth::{OrgId, UserId};
use crate::types::AvatarUrl;

use super::error::AgentStoreError;
use super::types::{
    AgentDescription, AgentId, AgentName, AgentRecord, AgentSeed, AgentSystemPrompt,
    AllowedMcpTools,
};

/// Input to [`AgentStore::create`]. Server-side fields (`id`, `created_at`,
/// `updated_at`) are minted by the store; never carried in.
#[derive(Debug, Clone)]
pub struct NewAgent {
    /// Owning organisation. Set by the HTTP handler from the request
    /// principal or by tool-driven creators from the caller agent's
    /// `org_id`; required because `agents.org_id` is `NOT NULL`.
    pub org_id: OrgId,
    pub name: AgentName,
    pub system_prompt: AgentSystemPrompt,
    /// Operator-curated, model-facing one-sentence blurb. Required at
    /// create time — there is no "empty description" path.
    pub description: AgentDescription,
    /// Initial MCP allowlist. Empty for the no-MCP-tools default; the operator
    /// supplies it explicitly when granting access at create time. Each
    /// server entry may carry `None` (= all of its tools) or `Some(set)`
    /// (= only those remote tool names).
    pub allowed_mcp_tools: AllowedMcpTools,
    /// Optional per-agent LLM model. `None` defers to the workspace default
    /// at agent-build time; `Some(model)` pins this agent to a specific
    /// catalog model (and, transitively, its provider).
    pub model: Option<crate::provider::Model>,
    /// Optional per-agent avatar URL. `None` leaves the column NULL (the
    /// default-app-avatar fallback). Validated as a shared [`AvatarUrl`]
    /// at the HTTP boundary before reaching the store.
    pub avatar_url: Option<AvatarUrl>,
    /// Principal who minted this agent, stamped onto the seeded v1
    /// `agent_prompt_versions` row. `None` for system-seeded agents
    /// (composition root, OAuth callback) where no user is in hand;
    /// `Some` for HTTP create and the `create_agent` tool.
    pub edited_by: Option<UserId>,
}

/// HTTP-PATCH-style update payload.
///
/// Each field's outer `Option` distinguishes "field omitted (no change)" from
/// "field present (set)". `allowed_mcp_tools = Some(<empty>)` is the lockdown
/// path — distinct from "field omitted" so HTTP PATCH can revoke every server.
#[derive(Debug, Clone, Default)]
pub struct AgentUpdate {
    pub name: Option<AgentName>,
    pub system_prompt: Option<AgentSystemPrompt>,
    /// Patch the description. Required, non-empty if present — the
    /// newtype's `TryFrom` rejects empty/whitespace at the HTTP boundary.
    pub description: Option<AgentDescription>,
    pub allowed_mcp_tools: Option<AllowedMcpTools>,
    /// Patch the per-agent model. Double-`Option` follows the same nullable-
    /// PATCH idiom as the HTTP layer: outer `None` = "field omitted, leave
    /// untouched", outer `Some(None)` = "explicitly clear back to the
    /// workspace default", outer `Some(Some(m))` = "pin to `m`".
    /// `clippy::option_option` is allowed because the tri-state is the whole
    /// reason this field exists — an enum split would inflate every caller.
    #[allow(clippy::option_option)]
    pub model: Option<Option<crate::provider::Model>>,
    /// Patch the per-agent avatar URL. Double-`Option` follows the same
    /// tri-state idiom as `model`: outer `None` = "field omitted, leave
    /// untouched", outer `Some(None)` = "clear back to NULL (default
    /// avatar)", outer `Some(Some(url))` = "set to this URL".
    #[allow(clippy::option_option)]
    pub avatar_url: Option<Option<AvatarUrl>>,
    /// Principal driving this edit. Stamped on the new
    /// `agent_prompt_versions` row when (and only when) the prompt or
    /// model actually changes. `None` is reserved for paths with no user
    /// principal in hand (rare — most updates are HTTP PATCH).
    pub edited_by: Option<UserId>,
}

/// Storage trait for the agents registry. Implementations must be thread-safe.
#[async_trait]
pub trait AgentStore: fmt::Debug + Send + Sync {
    /// Mint a new agent row.
    async fn create(&self, payload: NewAgent) -> Result<AgentRecord, AgentStoreError>;

    /// Tenant-scoped variant of [`Self::create`]. Opens
    /// `begin_as_user(acting_user_id)` so the `agents` INSERT runs
    /// RLS-checked — a tool acting on behalf of a foreign-org user
    /// is rejected at the WITH CHECK boundary. The `create_agent`
    /// tool sources `acting_user_id` from the claimed session's
    /// `created_by_user_id`; HTTP and seeder paths keep the
    /// privileged entry point.
    async fn create_for_user(
        &self,
        acting_user_id: UserId,
        payload: NewAgent,
    ) -> Result<AgentRecord, AgentStoreError>;

    /// Idempotent per-org preset seed: insert `seed` (the recruiter) for
    /// `org_id` if no agent of that name exists in the org. Returns the id
    /// of the resulting row, whether minted here or already present. Called
    /// from org creation so the fresh workspace has a usable agent
    /// immediately. The preset is an ordinary agent — there is no runtime
    /// "default" concept.
    async fn seed_preset(&self, org_id: OrgId, seed: AgentSeed)
    -> Result<AgentId, AgentStoreError>;

    /// Snapshot of every row, ordered by `created_at` ascending.
    async fn list(&self) -> Result<Vec<AgentRecord>, AgentStoreError>;

    /// Fetch a single agent by id.
    async fn read(&self, id: AgentId) -> Result<AgentRecord, AgentStoreError>;

    /// Patch the row with whatever subset of fields is set on `payload`.
    async fn update(
        &self,
        id: AgentId,
        payload: AgentUpdate,
    ) -> Result<AgentRecord, AgentStoreError>;

    /// Remove the row.
    async fn delete(&self, id: AgentId) -> Result<(), AgentStoreError>;

    /// Case-insensitive lookup by [`AgentName`] scoped to the viewer
    /// agent's org. Returns the matching record on success;
    /// [`AgentStoreError::NameNotFound`] when no row in the same org
    /// matches. Powers the model-facing addressing surfaces — the
    /// `send_message` tool resolves `{kind:"agent", name:<role>}` through
    /// here.
    async fn read_by_name_for_viewer(
        &self,
        viewer: AgentId,
        name: &AgentName,
    ) -> Result<AgentRecord, AgentStoreError>;

    /// Case-insensitive lookup by [`AgentName`] scoped directly to
    /// `org_id`. Same semantics as [`Self::read_by_name_for_viewer`] but
    /// for callers that have no viewer agent — notably the Slack bridge,
    /// which resolves `@AgentName` mentions before any in-DAG context
    /// exists.
    async fn read_by_name_for_org(
        &self,
        org_id: OrgId,
        name: &AgentName,
    ) -> Result<AgentRecord, AgentStoreError>;

    /// Snapshot of every `(id, name)` pair in `org_id`, ordered
    /// alphabetically by `lower(name)`. Used by callers that have an
    /// [`OrgId`] in hand without an in-DAG viewer — notably the Slack
    /// bridge's `/patom` slash command, which needs the tenant's agent
    /// roster to populate a Block Kit select menu before any session
    /// exists. Distinct from [`Self::list`] so the caller can skip
    /// hydrating columns it does not need.
    async fn list_for_org(
        &self,
        org_id: OrgId,
    ) -> Result<Vec<(AgentId, AgentName)>, AgentStoreError>;
}

/// Cheap-clone handle so collaborators can hold the store without a generic
/// parameter.
pub type SharedAgentStore = Arc<dyn AgentStore>;
