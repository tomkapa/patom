//! Storage trait + cheap-clone handle for the agents registry.

use std::fmt;
use std::sync::Arc;

use async_trait::async_trait;

use crate::auth::{OrgId, UserId};

use super::error::AgentStoreError;
use super::types::{
    AgentCard, AgentDescription, AgentId, AgentName, AgentRecord, AgentSystemPrompt,
    AllowedMcpTools, DefaultAgentSeed,
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
    /// When `true`, the new row becomes the default within `org_id`; the
    /// previously-default row in the same org is demoted in the same
    /// transaction so the partial unique index `agents_default_unique`
    /// stays satisfied.
    pub is_default: bool,
    /// Initial MCP allowlist. Empty for the no-MCP-tools default; the operator
    /// supplies it explicitly when granting access at create time. Each
    /// server entry may carry `None` (= all of its tools) or `Some(set)`
    /// (= only those remote tool names).
    pub allowed_mcp_tools: AllowedMcpTools,
    /// Optional per-agent LLM model. `None` defers to the workspace default
    /// at agent-build time; `Some(model)` pins this agent to a specific
    /// catalog model (and, transitively, its provider).
    pub model: Option<crate::provider::Model>,
    /// Principal who minted this agent, stamped onto the seeded v1
    /// `agent_prompt_versions` row. `None` for system-seeded agents
    /// (composition root, OAuth callback) where no user is in hand;
    /// `Some` for HTTP create and the `create_agent` tool.
    pub edited_by: Option<UserId>,
}

/// HTTP-PATCH-style update payload.
///
/// Each field's outer `Option` distinguishes "field omitted (no change)" from
/// "field present (set)". `is_default = Some(true)` triggers an atomic
/// demote-then-promote; `is_default = Some(false)` is rejected when applied to
/// the current default row (the system requires a default to exist at all
/// times). `allowed_mcp_tools = Some(<empty>)` is the lockdown path —
/// distinct from "field omitted" so HTTP PATCH can revoke every server.
#[derive(Debug, Clone, Default)]
pub struct AgentUpdate {
    pub name: Option<AgentName>,
    pub system_prompt: Option<AgentSystemPrompt>,
    /// Patch the description. Required, non-empty if present — the
    /// newtype's `TryFrom` rejects empty/whitespace at the HTTP boundary.
    pub description: Option<AgentDescription>,
    pub is_default: Option<bool>,
    pub allowed_mcp_tools: Option<AllowedMcpTools>,
    /// Patch the per-agent model. Double-`Option` follows the same nullable-
    /// PATCH idiom as the HTTP layer: outer `None` = "field omitted, leave
    /// untouched", outer `Some(None)` = "explicitly clear back to the
    /// workspace default", outer `Some(Some(m))` = "pin to `m`".
    /// `clippy::option_option` is allowed because the tri-state is the whole
    /// reason this field exists — an enum split would inflate every caller.
    #[allow(clippy::option_option)]
    pub model: Option<Option<crate::provider::Model>>,
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

    /// Idempotent per-org seed: insert `seed` as the default agent for
    /// `org_id` if no default row exists for that org. Returns the id of
    /// the resulting default row, whether minted here or already present.
    /// Called from the OAuth callback on first sign-up so the freshly
    /// minted personal org has a usable default agent immediately.
    async fn seed_default(
        &self,
        org_id: OrgId,
        seed: DefaultAgentSeed,
    ) -> Result<AgentId, AgentStoreError>;

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

    /// Remove the row. Refuses to delete the row currently flagged as default
    /// (returns [`AgentStoreError::DefaultDeletionForbidden`]).
    async fn delete(&self, id: AgentId) -> Result<(), AgentStoreError>;

    /// Id of the row flagged `is_default = TRUE` within `org_id`. Each
    /// org has exactly one default agent (enforced by the
    /// `agents_default_unique` partial unique index on
    /// `(org_id) WHERE is_default`), seeded when the org is created.
    async fn default_id_for(&self, org_id: OrgId) -> Result<AgentId, AgentStoreError>;

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

    /// Snapshot of every `(id, name)` pair in `viewer`'s org, ordered
    /// alphabetically by `lower(name)`. Used to render the `<agents>`
    /// block; the renderer excludes the caller before formatting.
    /// Distinct from [`Self::list`] so the caller can skip hydrating
    /// columns it does not need.
    async fn list_names_for_viewer(
        &self,
        viewer: AgentId,
    ) -> Result<Vec<(AgentId, AgentName)>, AgentStoreError>;

    /// Same shape as [`Self::list_names_for_viewer`] but scoped directly
    /// to `org_id`. Used by callers that have an [`OrgId`] in hand
    /// without an in-DAG viewer — notably the Slack bridge's `/patom`
    /// slash command, which needs the tenant's agent roster to populate
    /// a Block Kit select menu before any session exists.
    async fn list_for_org(
        &self,
        org_id: OrgId,
    ) -> Result<Vec<(AgentId, AgentName)>, AgentStoreError>;

    /// Top-K cosine-similarity search over agents' description
    /// embeddings, restricted to rows in the viewer's org. Returns slim
    /// cards sorted by descending similarity, capped at `k`, with
    /// `viewer` excluded at the SQL boundary so the caller never pays
    /// decode cost for the self row. Rows whose embedding is null (not
    /// yet backfilled) are skipped — the caller treats an empty result
    /// as a degraded layer rather than an error.
    async fn search_by_description(
        &self,
        embedding: &[f32],
        viewer: AgentId,
        k: usize,
    ) -> Result<Vec<AgentCard>, AgentStoreError>;
}

/// Cheap-clone handle so collaborators can hold the store without a generic
/// parameter.
pub type SharedAgentStore = Arc<dyn AgentStore>;
