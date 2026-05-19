//! CRUD endpoints for the agents registry.
//!
//! `POST   /agents`                  — create
//! `GET    /agents`                  — list
//! `GET    /agents/{id}`             — read one
//! `PUT    /agents/{id}`             — update
//! `DELETE /agents/{id}`             — delete (refuses the default; refuses any agent
//!                                    referenced by an existing session)
//! `GET    /agents/{id}/tool-calls`  — cursor-paginated audit list of the agent's
//!                                    recent tool invocations, joined to
//!                                    `mcp_servers` for the per-row connection chip.
//!
//! Caching: an update to `system_prompt` becomes visible to live workers within
//! [`crate::agents::AGENT_PROMPT_CACHE_TTL`] (60 s) — there is no synchronous
//! invalidation of the worker's prompt cache here, by design (see the design
//! conversation: "Live prompt + 60 s TTL, no LISTEN/NOTIFY").

use axum::Json;
use axum::Router;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{delete, get, post, put};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::types::Json as SqlxJson;
use uuid::Uuid;

use crate::agents::{
    AgentDescription, AgentId, AgentName, AgentRecord, AgentSystemPrompt, AgentUpdate,
    AllowedMcpTools, NewAgent,
};
use crate::auth::{AuthError, Principal, VisibilityTable, visible_to};
use crate::mcp::McpServerId;
use crate::tools::{DEFAULT_TOOL_CALLS_PAGE, MAX_TOOL_CALLS_PAGE, ToolCallRowId};

use super::super::error::HttpError;
use super::super::state::AppState;

pub(super) fn router() -> Router<AppState> {
    Router::new()
        .route("/agents", post(create_agent).get(list_agents))
        .route(
            "/agents/{id}",
            get(read_agent)
                .merge(put(update_agent))
                .merge(delete(delete_agent)),
        )
        .route("/agents/{id}/tool-calls", get(list_agent_tool_calls))
}

/// Wire shape returned on every agents endpoint. Mirrors the row plus
/// derived/server-managed fields.
#[derive(Debug, Serialize)]
struct AgentResponse {
    id: AgentId,
    name: String,
    system_prompt: String,
    /// Operator-curated, model-facing one-sentence blurb embedded for
    /// `search_agents`. Always present — the column is `NOT NULL`.
    description: String,
    is_default: bool,
    /// Per-agent MCP tool allowlist, keyed by server id. `null` value =
    /// every tool from that server; otherwise the explicit list of remote
    /// tool names. Always present; an empty object means the agent has no
    /// MCP access (the default for newly minted agents).
    allowed_mcp_tools: AllowedMcpTools,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

impl From<AgentRecord> for AgentResponse {
    fn from(r: AgentRecord) -> Self {
        Self {
            id: r.id,
            name: r.name.as_str().to_owned(),
            system_prompt: r.system_prompt.as_str().to_owned(),
            description: r.description.as_str().to_owned(),
            is_default: r.is_default,
            allowed_mcp_tools: r.allowed_mcp_tools,
            created_at: r.created_at,
            updated_at: r.updated_at,
        }
    }
}

#[derive(Debug, Deserialize)]
struct CreateAgentRequest {
    name: String,
    system_prompt: String,
    /// Required, non-empty (doc/agent_discovery_plan.md §5.2). Embedded
    /// for `search_agents`.
    description: String,
    /// When `true`, the new agent becomes the default. The previously-default
    /// row is demoted in the same transaction.
    #[serde(default)]
    is_default: bool,
    /// MCP tools the new agent may use, keyed by server id. `null` =
    /// every tool from that server; otherwise the explicit list of remote
    /// tool names. Omitted = no MCP access (`{}`): there is no
    /// "unrestricted" mode. The operator opts in explicitly.
    #[serde(default)]
    allowed_mcp_tools: AllowedMcpTools,
}

#[derive(Debug, Deserialize)]
struct UpdateAgentRequest {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    system_prompt: Option<String>,
    /// Patch the description. `Some(_)` re-embeds; `None` (field omitted)
    /// leaves the existing description and embedding untouched.
    #[serde(default)]
    description: Option<String>,
    /// `Some(true)` promotes this row to default (atomically demotes the
    /// previous default). `Some(false)` is rejected when applied to the
    /// current default — the system requires exactly one default at all times.
    #[serde(default)]
    is_default: Option<bool>,
    /// `Some(map)` replaces the allowlist atomically — including
    /// `Some({})`, the explicit lockdown that revokes every server. `None`
    /// (field omitted) leaves the existing allowlist untouched.
    #[serde(default)]
    allowed_mcp_tools: Option<AllowedMcpTools>,
}

async fn create_agent(
    State(state): State<AppState>,
    principal: Principal,
    Json(payload): Json<CreateAgentRequest>,
) -> Result<(StatusCode, Json<AgentResponse>), HttpError> {
    let name = AgentName::try_from(payload.name).map_err(HttpError::Parse)?;
    let system_prompt =
        AgentSystemPrompt::try_from(payload.system_prompt).map_err(HttpError::Parse)?;
    let description = AgentDescription::try_from(payload.description).map_err(HttpError::Parse)?;
    let record = state
        .agents
        .create(NewAgent {
            org_id: principal.active_org_id,
            name,
            system_prompt,
            description,
            is_default: payload.is_default,
            allowed_mcp_tools: payload.allowed_mcp_tools,
        })
        .await?;
    Ok((StatusCode::CREATED, Json(record.into())))
}

async fn list_agents(
    State(state): State<AppState>,
    principal: Principal,
) -> Result<Json<Vec<AgentResponse>>, HttpError> {
    // Tenant-scoped read: open a tx, set `app.user_id` via the GUC, and
    // let the `agents_org_isolation` RLS policy do the filtering. Mirrors
    // the mcp_servers route — bypasses the store's privileged read path
    // so the user can see only their own org's rows.
    let mut tx = crate::auth::begin_as(&state.pool, &principal).await?;
    let rows = sqlx::query_as::<_, AgentRowForList>(
        "SELECT id, org_id, name, system_prompt, description, is_default, \
                allowed_mcp_tools, created_at, updated_at \
         FROM agents ORDER BY created_at ASC",
    )
    .fetch_all(&mut *tx)
    .await
    .map_err(AuthError::from)?;
    tx.commit().await.map_err(AuthError::from)?;
    let out = rows
        .into_iter()
        .map(AgentRowForList::into_response)
        .collect();
    Ok(Json(out))
}

async fn read_agent(
    State(state): State<AppState>,
    principal: Principal,
    Path(id): Path<Uuid>,
) -> Result<Json<AgentResponse>, HttpError> {
    let id = AgentId::from(id);
    let mut tx = crate::auth::begin_as(&state.pool, &principal).await?;
    let row = sqlx::query_as::<_, AgentRowForList>(
        "SELECT id, org_id, name, system_prompt, description, is_default, \
                allowed_mcp_tools, created_at, updated_at \
         FROM agents WHERE id = $1",
    )
    .bind(id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(AuthError::from)?;
    tx.commit().await.map_err(AuthError::from)?;
    let row = row.ok_or(HttpError::NotFound)?;
    Ok(Json(row.into_response()))
}

async fn update_agent(
    State(state): State<AppState>,
    principal: Principal,
    Path(id): Path<Uuid>,
    Json(payload): Json<UpdateAgentRequest>,
) -> Result<Json<AgentResponse>, HttpError> {
    let id = AgentId::from(id);
    let name = payload
        .name
        .map(AgentName::try_from)
        .transpose()
        .map_err(HttpError::Parse)?;
    let system_prompt = payload
        .system_prompt
        .map(AgentSystemPrompt::try_from)
        .transpose()
        .map_err(HttpError::Parse)?;
    let description = payload
        .description
        .map(AgentDescription::try_from)
        .transpose()
        .map_err(HttpError::Parse)?;
    let allowed_mcp_tools = payload.allowed_mcp_tools;
    // Tenant gate: 404 cross-org / unknown ids without leaking existence
    // before dispatching the privileged update.
    if !crate::auth::visible_to(
        &state.pool,
        &principal,
        crate::auth::VisibilityTable::Agents,
        id.as_uuid(),
    )
    .await?
    {
        return Err(HttpError::NotFound);
    }
    let row = state
        .agents
        .update(
            id,
            AgentUpdate {
                name,
                system_prompt,
                description,
                is_default: payload.is_default,
                allowed_mcp_tools,
            },
        )
        .await?;
    Ok(Json(row.into()))
}

async fn delete_agent(
    State(state): State<AppState>,
    principal: Principal,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, HttpError> {
    let id = AgentId::from(id);
    if !crate::auth::visible_to(
        &state.pool,
        &principal,
        crate::auth::VisibilityTable::Agents,
        id.as_uuid(),
    )
    .await?
    {
        return Err(HttpError::NotFound);
    }
    state.agents.delete(id).await?;
    Ok(StatusCode::NO_CONTENT)
}

// Local row type for the tenant-scoped SELECTs. Mirrors the columns
// returned by the store's read path but lives here so the route can run
// raw SQL inside the principal-scoped tx without going through the
// store's privileged transaction.
#[derive(sqlx::FromRow)]
struct AgentRowForList {
    id: AgentId,
    org_id: crate::auth::OrgId,
    name: String,
    system_prompt: String,
    description: String,
    is_default: bool,
    allowed_mcp_tools: SqlxJson<AllowedMcpTools>,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

impl AgentRowForList {
    fn into_response(self) -> AgentResponse {
        let _ = self.org_id; // not on the wire shape — RLS already filtered.
        AgentResponse {
            id: self.id,
            name: self.name,
            system_prompt: self.system_prompt,
            description: self.description,
            is_default: self.is_default,
            allowed_mcp_tools: self.allowed_mcp_tools.0,
            created_at: self.created_at,
            updated_at: self.updated_at,
        }
    }
}

// ────────────────────────────────────────────────────────────────────────
// Per-agent tool-call audit list
// ────────────────────────────────────────────────────────────────────────
//
// Mirrors the per-server endpoint in `mcp.rs` for shape/cursor semantics,
// but pivots on `agent_id` (covered by the `tool_calls_per_agent_mcp_idx`
// index from migration 25). Joined to `mcp_servers` to project the
// per-row connection id + alias. Scoped to MCP traffic by
// `tc.mcp_server_id IS NOT NULL` — `tool_calls` is shared with system
// tools (send_message, search_agents, …) that record a null
// `mcp_server_id`, and rows from deleted connections (`ON DELETE SET
// NULL`) collapse to the same shape. Both drop out here so the per-agent
// "Recent activity" panel renders only resolvable connection chips.

#[derive(Debug, Deserialize)]
struct AgentToolCallsQuery {
    /// Defaults to [`DEFAULT_TOOL_CALLS_PAGE`], clamped to
    /// `1..=MAX_TOOL_CALLS_PAGE` by the handler.
    limit: Option<u16>,
    /// Exclusive `started_at` cursor — returned rows have `started_at < before`.
    /// Pass the previous page's `next_cursor` to walk backwards in time.
    before: Option<DateTime<Utc>>,
}

#[derive(Debug, Serialize, sqlx::FromRow)]
struct AgentToolCallResponse {
    id: ToolCallRowId,
    tool_name: String,
    /// LEFT JOIN: the audit row outlives its connection if the
    /// `mcp_servers` row is ever deleted (`ON DELETE SET NULL`).
    mcp_server_id: Option<McpServerId>,
    /// LEFT JOIN: nullable for the same reason as `mcp_server_id`.
    mcp_server_alias: Option<String>,
    started_at: DateTime<Utc>,
    duration_ms: i32,
    is_error: bool,
    /// Non-null only when `is_error = true` (migration-27 CHECK).
    error_message: Option<String>,
}

#[derive(Debug, Serialize)]
struct AgentToolCallListResponse {
    items: Vec<AgentToolCallResponse>,
    /// `Some(ts)` when more rows exist beyond this page — pass it back as
    /// `?before=` to fetch the next slice. `None` when the page is the tail.
    next_cursor: Option<DateTime<Utc>>,
}

async fn list_agent_tool_calls(
    State(state): State<AppState>,
    principal: Principal,
    Path(id): Path<Uuid>,
    Query(params): Query<AgentToolCallsQuery>,
) -> Result<Json<AgentToolCallListResponse>, HttpError> {
    let agent_id = AgentId::from(id);
    // Pre-gate visibility so a foreign / unknown id 404s with the same
    // shape as `read_agent`, without leaking existence through an empty
    // list response.
    if !visible_to(&state.pool, &principal, VisibilityTable::Agents, id).await? {
        return Err(HttpError::NotFound);
    }

    let limit = params
        .limit
        .unwrap_or(DEFAULT_TOOL_CALLS_PAGE)
        .clamp(1, MAX_TOOL_CALLS_PAGE);
    // Fetch one extra row to detect "has more" without committing it to
    // this page. `i64` because sqlx binds `LIMIT` through a bigint param.
    let fetch_limit = i64::from(limit) + 1;

    let mut tx = crate::auth::begin_as(&state.pool, &principal).await?;
    // `tc.mcp_server_id IS NOT NULL` scopes the audit list to MCP traffic.
    // The `tool_calls` table is shared with system tools (send_message,
    // search_agents, …) which record rows with a null `mcp_server_id`; the
    // per-agent "Recent activity" panel surfaces connection chips, so
    // those rows are filtered out at the source. Rows whose connection was
    // deleted (`ON DELETE SET NULL`) also drop out — acceptable because
    // the chip would render as "Removed connection" with no usable target.
    let mut items = sqlx::query_as::<_, AgentToolCallResponse>(
        "SELECT tc.id, tc.tool_name, tc.mcp_server_id, s.alias AS mcp_server_alias, \
                tc.started_at, tc.duration_ms, tc.is_error, tc.error_message \
         FROM tool_calls tc \
         JOIN mcp_servers s ON s.id = tc.mcp_server_id \
         WHERE tc.agent_id = $1 \
           AND tc.mcp_server_id IS NOT NULL \
           AND ($2::timestamptz IS NULL OR tc.started_at < $2) \
         ORDER BY tc.started_at DESC \
         LIMIT $3",
    )
    .bind(agent_id)
    .bind(params.before)
    .bind(fetch_limit)
    .fetch_all(&mut *tx)
    .await
    .map_err(AuthError::from)?;
    tx.commit().await.map_err(AuthError::from)?;

    let has_more = items.len() > usize::from(limit);
    if has_more {
        items.pop();
    }
    let next_cursor = has_more
        .then(|| items.last().map(|r| r.started_at))
        .flatten();

    Ok(Json(AgentToolCallListResponse { items, next_cursor }))
}
