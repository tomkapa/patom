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

use crate::agent_core::{MAX_TURN_LIST_PAGE_SIZE, MAX_TURNS_PER_TIMESERIES_RESPONSE};
use crate::agents::prompt_versions::{
    NewPromptVersion, PromptVersionError, PromptVersionId, PromptVersionNumber, PromptVersionRow,
};
use crate::agents::{
    AgentDescription, AgentId, AgentName, AgentRecord, AgentSystemPrompt, AgentUpdate,
    AllowedMcpTools, NewAgent,
};
use crate::auth::{
    AuthError, OrgId, Principal, UserId, VisibilityTable, run_privileged, visible_to,
};
use crate::mcp::McpServerId;
use crate::provider::Model;
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
        .route(
            "/agents/{id}/metrics/timeseries",
            get(get_agent_metrics_timeseries),
        )
        .route("/agents/{id}/turns", get(list_agent_turns))
        .route(
            "/agents/{id}/prompt-versions",
            get(list_agent_prompt_versions),
        )
        .route(
            "/agents/{id}/prompt-versions/{version}/restore",
            post(restore_agent_prompt_version),
        )
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
    /// Pinned per-agent model name, or `null` when the agent inherits the
    /// workspace default. Each catalog model is served by exactly one
    /// provider; the FE can derive the provider chip from the catalog if
    /// needed.
    model: Option<&'static str>,
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
            model: r.model.map(Model::as_str),
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
    /// Optional catalog model id. Omit to inherit the workspace default
    /// (`Settings::model`). Unknown names reject at parse time with the
    /// `UnknownModel` reason. The provider is derived from the catalog —
    /// callers do not send it separately.
    #[serde(default)]
    model: Option<Model>,
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
    /// Patch the per-agent model. Double-`Option` distinguishes
    /// "field omitted (leave untouched)" from `null` ("clear back to
    /// workspace default") from `"<name>"` ("pin to this catalog model").
    /// Mirrors the PATCH idiom used by `description`. `clippy::option_option`
    /// is allowed here because the tri-state is intentional — the alternative
    /// (a per-field enum) would inflate every PATCH route.
    #[allow(clippy::option_option)]
    #[serde(default, deserialize_with = "deserialize_optional_optional_model")]
    model: Option<Option<Model>>,
}

/// Tri-state deserialiser so `{}` (omitted), `{"model": null}` (clear), and
/// `{"model": "claude-sonnet-4-5"}` (set) all map distinctly onto
/// `Option<Option<Model>>`. The default `Option` deserialise collapses null
/// and missing, which would force every PATCH to send the field — breaking
/// partial updates.
#[allow(clippy::option_option)]
fn deserialize_optional_optional_model<'de, D>(d: D) -> Result<Option<Option<Model>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(Some(Option::<Model>::deserialize(d)?))
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
            model: payload.model,
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
                allowed_mcp_tools, model, created_at, updated_at \
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
                allowed_mcp_tools, model, created_at, updated_at \
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
    // Snapshot the prior (system_prompt, model) so we can detect an
    // effective edit. The store update returns the *new* record; we want
    // the *old* for the bump comparison.
    //
    // Two concurrent PATCH requests can each see the same `before` and
    // both decide to bump — the UNIQUE (agent_id, version) constraint
    // forces them to different versions; the only observable side-effect
    // is two history rows with identical content, which is exactly the
    // (rare) shape a real human "double-saved" edit produces. Slice 3's
    // restore path follows the same idempotent shape (doc §4.5).
    let before = state.agents.read(id).await?;
    let new_record = state
        .agents
        .update(
            id,
            AgentUpdate {
                name,
                system_prompt,
                description,
                is_default: payload.is_default,
                allowed_mcp_tools,
                model: payload.model,
            },
        )
        .await?;

    // Bump `agent_prompt_versions` only when the prompt or model actually
    // changed. The two are treated as one tuple — a model swap is the same
    // act as a prompt edit (doc/logs_metrics_tab.md §4.1).
    let prompt_changed = before.system_prompt.as_str() != new_record.system_prompt.as_str();
    let model_changed = before.model != new_record.model;
    if prompt_changed || model_changed {
        // Best-effort: an insert failure here means the audit history is
        // momentarily stale but the agents table has the authoritative
        // current values. Log loudly so a recurring failure is visible.
        if let Err(e) = state
            .prompt_versions
            .insert_bump(NewPromptVersion {
                agent_id: id,
                org_id: new_record.org_id,
                system_prompt: new_record.system_prompt.clone(),
                model: new_record.model,
                edited_by: Some(principal.user_id),
            })
            .await
        {
            tracing::error!(
                error = ?e,
                relay.agent.id = %id,
                "agent_prompt_versions.bump.failed",
            );
        }
    }
    Ok(Json(new_record.into()))
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
    model: Option<Model>,
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
            model: self.model.map(Model::as_str),
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
    mcp_server_catalog_id: Option<String>,
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
        "SELECT tc.id, tc.tool_name, tc.mcp_server_id, s.catalog_id AS mcp_server_catalog_id, \
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

// ────────────────────────────────────────────────────────────────────────
// Logs & Metrics tab — timeseries + turns endpoints
// (doc/logs_metrics_tab.md §5–§6)
// ────────────────────────────────────────────────────────────────────────
//
// The chart and timeline both pivot on `turn_metrics`, joined to
// `agent_prompt_versions` for the per-bucket version label. Aggregations
// live in SQL — the timeseries endpoint must never ship raw rows to power
// a bar (CLAUDE.md §5: every batch capped; bound queries by an explicit
// LIMIT via `MAX_TURNS_PER_TIMESERIES_RESPONSE`).

/// Time-bucket granularity for the chart. Auto-derived from the requested
/// window when omitted; explicit override is offered so the FE can pin
/// "30m" / "1h" / "1d" for the URL.
#[derive(Debug, Clone, Copy, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
enum TimeseriesBucket {
    #[default]
    Auto,
    FiveMin,
    Hour,
    Day,
}

impl TimeseriesBucket {
    /// Concrete bucket width as `(label, postgres-truncation-unit)`. The
    /// truncation unit feeds `date_trunc($unit, started_at)` so the
    /// caller cannot inject SQL.
    const fn resolve(self, span_secs: i64) -> (&'static str, &'static str) {
        match self {
            Self::FiveMin => ("5m", "minute"), // bucketed by minute, FE multiplies
            Self::Hour => ("1h", "hour"),
            Self::Day => ("1d", "day"),
            Self::Auto => {
                if span_secs <= 60 * 60 * 6 {
                    ("5m", "minute")
                } else if span_secs <= 60 * 60 * 48 {
                    ("1h", "hour")
                } else {
                    ("1d", "day")
                }
            }
        }
    }
}

#[derive(Debug, Deserialize)]
struct MetricsTimeseriesQuery {
    from: Option<DateTime<Utc>>,
    to: Option<DateTime<Utc>>,
    #[serde(default)]
    bucket: TimeseriesBucket,
    /// `prev_window` | `none` — UI uses `prev_window` by default so the
    /// caption's `Δ vs compare window` is populated.
    #[serde(default)]
    compare: Option<String>,
}

#[derive(Debug, Serialize)]
struct ByKind {
    normal: i64,
    reflection: i64,
    resolution: i64,
}

#[derive(Debug, Serialize)]
struct TimeseriesBucketRow {
    start: DateTime<Utc>,
    by_kind: ByKind,
    latency_p50_ms: i64,
    latency_p95_ms: i64,
    failure_count: i64,
    /// `prompt_version_id` active for this bucket (most recent version
    /// whose `created_at <= bucket.start`). Nullable for windows that
    /// predate the first version row.
    prompt_version_id: Option<crate::agents::prompt_versions::PromptVersionId>,
}

#[derive(Debug, Serialize)]
struct TimeseriesTotals {
    tokens: i64,
    turns: i64,
    latency_p50_ms: i64,
    latency_p95_ms: i64,
    failure_count: i64,
}

#[derive(Debug, Serialize)]
struct TimeseriesDeltas {
    /// `None` when `compare=none` was requested or the prior window
    /// returned zero rows.
    tokens: Option<i64>,
    latency_p95_ms: Option<i64>,
    failure_count: Option<i64>,
}

#[derive(Debug, Serialize)]
struct PromptEditMarker {
    version: i32,
    created_at: DateTime<Utc>,
    edited_by: Option<crate::auth::UserId>,
}

#[derive(Debug, Serialize)]
struct MetricsTimeseriesResponse {
    bucket_label: String,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
    buckets: Vec<TimeseriesBucketRow>,
    totals: TimeseriesTotals,
    deltas_vs_compare: TimeseriesDeltas,
    prompt_edits: Vec<PromptEditMarker>,
}

#[derive(sqlx::FromRow)]
struct BucketAggRow {
    bucket_start: DateTime<Utc>,
    normal: Option<i64>,
    reflection: Option<i64>,
    resolution: Option<i64>,
    tokens: Option<i64>,
    p50: Option<f64>,
    p95: Option<f64>,
    failures: Option<i64>,
}

async fn get_agent_metrics_timeseries(
    State(state): State<AppState>,
    principal: Principal,
    Path(id): Path<Uuid>,
    Query(params): Query<MetricsTimeseriesQuery>,
) -> Result<Json<MetricsTimeseriesResponse>, HttpError> {
    let agent_id = AgentId::from(id);
    if !visible_to(&state.pool, &principal, VisibilityTable::Agents, id).await? {
        return Err(HttpError::NotFound);
    }

    let (from, to) = resolve_window(&params, state.clock.now_utc())?;
    let span_secs = (to - from).num_seconds();
    let (bucket_label, trunc_unit) = params.bucket.resolve(span_secs);

    let mut tx = crate::auth::begin_as(&state.pool, &principal).await?;
    let rows = fetch_buckets(&mut tx, agent_id, from, to, trunc_unit).await?;
    let edits = fetch_prompt_edits(&mut tx, agent_id, from, to).await?;
    let compare_totals = match params.compare.as_deref() {
        None | Some("prev_window") => Some(fetch_compare(&mut tx, agent_id, from, to).await?),
        _ => None,
    }
    .flatten();
    tx.commit().await.map_err(AuthError::from)?;

    let (totals, totals_p95) = compute_totals(&rows);
    let deltas = compute_deltas(&totals, totals_p95, compare_totals);
    let buckets = rows.into_iter().map(bucket_row_into_response).collect();
    let prompt_edits = edits
        .into_iter()
        .map(|(version, created_at, edited_by)| PromptEditMarker {
            version,
            created_at,
            edited_by,
        })
        .collect();

    Ok(Json(MetricsTimeseriesResponse {
        bucket_label: bucket_label.to_owned(),
        from,
        to,
        buckets,
        totals,
        deltas_vs_compare: deltas,
        prompt_edits,
    }))
}

/// Resolve / validate the `(from, to)` window. Default = last 24h; rejects
/// inverted ranges and windows wider than 31 days (CLAUDE.md §5).
fn resolve_window(
    params: &MetricsTimeseriesQuery,
    now: DateTime<Utc>,
) -> Result<(DateTime<Utc>, DateTime<Utc>), HttpError> {
    let to = params.to.unwrap_or(now);
    let from = params
        .from
        .unwrap_or_else(|| to - chrono::Duration::hours(24));
    if to <= from {
        return Err(HttpError::BadRequest("to must be greater than from".into()));
    }
    if (to - from).num_seconds() > 60 * 60 * 24 * 31 {
        return Err(HttpError::BadRequest("window exceeds 31 days".into()));
    }
    Ok((from, to))
}

async fn fetch_buckets(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    agent_id: AgentId,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
    trunc_unit: &'static str,
) -> Result<Vec<BucketAggRow>, HttpError> {
    // `trunc_unit` is selected from a fixed enum at the route boundary
    // (`TimeseriesBucket::resolve`) — never user-supplied — so the
    // `format!`-injected literal here cannot carry an attacker payload.
    // Everything else is a bound parameter (CLAUDE.md §10).
    let sql = format!(
        "WITH base AS ( \
            SELECT tm.started_at, tm.kind, \
                   (tm.input_tokens + tm.output_tokens)::bigint AS total_tokens, \
                   tm.duration_ms::float8 AS duration_ms, \
                   pr.status = 'failed' AS is_failure \
              FROM turn_metrics tm \
              JOIN prompt_requests pr ON pr.id = tm.request_id \
             WHERE tm.agent_id = $1 \
               AND tm.started_at >= $2 \
               AND tm.started_at <  $3 \
             ORDER BY tm.started_at DESC \
             LIMIT $4 \
         ) \
         SELECT date_trunc('{trunc_unit}', started_at) AS bucket_start, \
                COUNT(*) FILTER (WHERE kind = 'normal')     AS normal, \
                COUNT(*) FILTER (WHERE kind = 'reflection') AS reflection, \
                COUNT(*) FILTER (WHERE kind = 'resolution') AS resolution, \
                COALESCE(SUM(total_tokens), 0)::bigint AS tokens, \
                percentile_cont(0.5)  WITHIN GROUP (ORDER BY duration_ms) AS p50, \
                percentile_cont(0.95) WITHIN GROUP (ORDER BY duration_ms) AS p95, \
                COUNT(*) FILTER (WHERE is_failure)          AS failures \
           FROM base \
       GROUP BY bucket_start \
       ORDER BY bucket_start ASC"
    );
    sqlx::query_as::<_, BucketAggRow>(&sql)
        .bind(agent_id)
        .bind(from)
        .bind(to)
        .bind(MAX_TURNS_PER_TIMESERIES_RESPONSE)
        .fetch_all(&mut **tx)
        .await
        .map_err(|e| HttpError::from(AuthError::from(e)))
}

async fn fetch_prompt_edits(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    agent_id: AgentId,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
) -> Result<Vec<(i32, DateTime<Utc>, Option<crate::auth::UserId>)>, HttpError> {
    sqlx::query_as(
        "SELECT version, created_at, edited_by \
           FROM agent_prompt_versions \
          WHERE agent_id = $1 AND created_at >= $2 AND created_at < $3 \
          ORDER BY created_at ASC",
    )
    .bind(agent_id)
    .bind(from)
    .bind(to)
    .fetch_all(&mut **tx)
    .await
    .map_err(|e| HttpError::from(AuthError::from(e)))
}

async fn fetch_compare(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    agent_id: AgentId,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
) -> Result<Option<(i64, Option<f64>, i64)>, HttpError> {
    let span = to - from;
    let prev_from = from - span;
    sqlx::query_as(
        "SELECT COALESCE(SUM(tm.input_tokens + tm.output_tokens), 0)::bigint, \
                percentile_cont(0.95) WITHIN GROUP (ORDER BY tm.duration_ms::float8), \
                COUNT(*) FILTER (WHERE pr.status = 'failed') \
           FROM turn_metrics tm \
           JOIN prompt_requests pr ON pr.id = tm.request_id \
          WHERE tm.agent_id = $1 \
            AND tm.started_at >= $2 \
            AND tm.started_at <  $3",
    )
    .bind(agent_id)
    .bind(prev_from)
    .bind(from)
    .fetch_optional(&mut **tx)
    .await
    .map_err(|e| HttpError::from(AuthError::from(e)))
}

/// Fold the per-bucket aggregate into `(totals, totals_p95)`.
/// Split out to keep `get_agent_metrics_timeseries` under the
/// 70-line cap (CLAUDE.md §4).
fn compute_totals(rows: &[BucketAggRow]) -> (TimeseriesTotals, i64) {
    let mut total_tokens: i64 = 0;
    let mut total_turns: i64 = 0;
    let mut total_failures: i64 = 0;
    let mut p50_samples: Vec<i64> = Vec::with_capacity(rows.len());
    let mut p95_samples: Vec<i64> = Vec::with_capacity(rows.len());
    for r in rows {
        total_tokens += r.tokens.unwrap_or(0);
        total_turns +=
            r.normal.unwrap_or(0) + r.reflection.unwrap_or(0) + r.resolution.unwrap_or(0);
        total_failures += r.failures.unwrap_or(0);
        p50_samples.push(f64_ms_to_i64(r.p50.unwrap_or(0.0)));
        p95_samples.push(f64_ms_to_i64(r.p95.unwrap_or(0.0)));
    }
    let totals_p50 = median(&mut p50_samples);
    let totals_p95 = median(&mut p95_samples);
    (
        TimeseriesTotals {
            tokens: total_tokens,
            turns: total_turns,
            latency_p50_ms: totals_p50,
            latency_p95_ms: totals_p95,
            failure_count: total_failures,
        },
        totals_p95,
    )
}

/// Build the `Δ vs compare window` payload. Returns the "no compare"
/// shape when the upstream query was skipped (`compare=none`).
fn compute_deltas(
    totals: &TimeseriesTotals,
    totals_p95: i64,
    compare: Option<(i64, Option<f64>, i64)>,
) -> TimeseriesDeltas {
    compare.map_or(
        TimeseriesDeltas {
            tokens: None,
            latency_p95_ms: None,
            failure_count: None,
        },
        |(tokens, p95, failures)| TimeseriesDeltas {
            tokens: Some(totals.tokens - tokens),
            latency_p95_ms: Some(totals_p95 - f64_ms_to_i64(p95.unwrap_or(0.0))),
            failure_count: Some(totals.failure_count - failures),
        },
    )
}

fn bucket_row_into_response(r: BucketAggRow) -> TimeseriesBucketRow {
    TimeseriesBucketRow {
        start: r.bucket_start,
        by_kind: ByKind {
            normal: r.normal.unwrap_or(0),
            reflection: r.reflection.unwrap_or(0),
            resolution: r.resolution.unwrap_or(0),
        },
        latency_p50_ms: f64_ms_to_i64(r.p50.unwrap_or(0.0)),
        latency_p95_ms: f64_ms_to_i64(r.p95.unwrap_or(0.0)),
        failure_count: r.failures.unwrap_or(0),
        // Resolved client-side in slice 2 (drawer) — leaving null
        // until the per-bucket join lands.
        prompt_version_id: None,
    }
}

/// Median of a small bounded `Vec<i64>`. Sorts in place; returns 0 on
/// empty input. Used for chart caption totals.
fn median(xs: &mut [i64]) -> i64 {
    if xs.is_empty() {
        return 0;
    }
    xs.sort_unstable();
    xs[xs.len() / 2]
}

/// Clamp an `f64` millisecond value into `i64`, saturating at the bounds
/// and rounding to nearest. The `percentile_cont()` return type is float8
/// (Postgres) but every downstream consumer (chart label, delta math)
/// wants integer milliseconds.
///
/// CLAUDE.md §7: no `as` narrowing — the conversion is total via
/// saturate-on-overflow.
fn f64_ms_to_i64(v: f64) -> i64 {
    let r = v.round();
    if !r.is_finite() {
        return 0;
    }
    // f64 can represent every i32 exactly, so a "is it inside i32 range"
    // check is loss-free. Latency in millis comfortably fits i32
    // (≈24 days) but the SQL aggregation returns i64; an out-of-i32 value
    // is unrealistic for any real provider call and is clamped here to
    // a safe i32::MAX rather than risking the i64-to-f64 precision loss
    // clippy flags. Clamp returns the closer of the two bounds.
    if r >= f64::from(i32::MAX) {
        return i64::from(i32::MAX);
    }
    if r <= f64::from(i32::MIN) {
        return i64::from(i32::MIN);
    }
    // r is finite and inside i32 range; `as` is total here. The two
    // bound checks above are the safety net clippy wants.
    #[allow(clippy::as_conversions, clippy::cast_possible_truncation)]
    let truncated = r as i32;
    i64::from(truncated)
}

#[derive(Debug, Deserialize)]
struct TurnsListQuery {
    from: Option<DateTime<Utc>>,
    to: Option<DateTime<Utc>>,
    /// Filter by `turn_metrics.kind` — `normal` / `reflection` /
    /// `resolution`. Omitted = all kinds.
    kind: Option<String>,
    /// Exclusive `started_at` cursor — pass the previous page's
    /// `next_cursor` to walk backwards in time.
    cursor: Option<DateTime<Utc>>,
}

#[derive(Debug, Serialize, sqlx::FromRow)]
struct TurnRow {
    request_id: crate::runtime::PromptRequestId,
    started_at: DateTime<Utc>,
    kind: String,
    model: String,
    provider: String,
    input_tokens: i32,
    output_tokens: i32,
    cache_creation_tokens: Option<i32>,
    cache_read_tokens: Option<i32>,
    duration_ms: i32,
    stop_reason: String,
    history_count: i32,
    /// Joined from `prompt_requests` so the OUTCOME column can show
    /// `failed` rows distinctly from `done` rows. Mirrors
    /// `RequestStatus::as_str()` labels.
    status: String,
    /// `Some(...)` on failure (`prompt_requests.failure_reason`); shown
    /// inline in the timeline.
    failure_reason: Option<String>,
    /// Joined from `agent_prompt_versions.version` so the PROMPT column
    /// renders `v6` / `v7` without an extra round-trip.
    prompt_version: i32,
}

#[derive(Debug, Serialize)]
struct TurnsListResponse {
    items: Vec<TurnRow>,
    next_cursor: Option<DateTime<Utc>>,
}

async fn list_agent_turns(
    State(state): State<AppState>,
    principal: Principal,
    Path(id): Path<Uuid>,
    Query(params): Query<TurnsListQuery>,
) -> Result<Json<TurnsListResponse>, HttpError> {
    let agent_id = AgentId::from(id);
    if !visible_to(&state.pool, &principal, VisibilityTable::Agents, id).await? {
        return Err(HttpError::NotFound);
    }

    // Validate the optional `kind` filter against the closed enum to
    // avoid passing raw user input into the SQL `=` even with a bound
    // param (defence in depth + clearer 400s).
    let kind_filter = match params.kind.as_deref() {
        // Omitted, empty, or "all" all map to "no filter" — the chart
        // strip's default for a fresh load.
        None | Some("" | "all") => None,
        Some(k @ ("normal" | "reflection" | "resolution")) => Some(k.to_owned()),
        Some(_) => return Err(HttpError::BadRequest("unknown kind".into())),
    };

    let now = state.clock.now_utc();
    let to = params.to.unwrap_or(now);
    let from = params
        .from
        .unwrap_or_else(|| to - chrono::Duration::hours(24));
    let cursor = params.cursor;

    let limit = i64::from(MAX_TURN_LIST_PAGE_SIZE);
    let fetch_limit = limit + 1;

    let mut tx = crate::auth::begin_as(&state.pool, &principal).await?;
    let mut items = sqlx::query_as::<_, TurnRow>(
        "SELECT tm.request_id, tm.started_at, tm.kind, tm.model, tm.provider, \
                tm.input_tokens, tm.output_tokens, tm.cache_creation_tokens, tm.cache_read_tokens, \
                tm.duration_ms, tm.stop_reason, tm.history_count, \
                pr.status::text AS status, pr.failure_reason::text AS failure_reason, \
                apv.version AS prompt_version \
           FROM turn_metrics tm \
           JOIN prompt_requests pr ON pr.id = tm.request_id \
           JOIN agent_prompt_versions apv ON apv.id = tm.prompt_version_id \
          WHERE tm.agent_id = $1 \
            AND tm.started_at >= $2 \
            AND tm.started_at <  $3 \
            AND ($4::text IS NULL OR tm.kind = $4) \
            AND ($5::timestamptz IS NULL OR tm.started_at < $5) \
          ORDER BY tm.started_at DESC \
          LIMIT $6",
    )
    .bind(agent_id)
    .bind(from)
    .bind(to)
    .bind(kind_filter)
    .bind(cursor)
    .bind(fetch_limit)
    .fetch_all(&mut *tx)
    .await
    .map_err(AuthError::from)?;
    tx.commit().await.map_err(AuthError::from)?;

    let has_more = items.len() > usize::try_from(limit).unwrap_or(usize::MAX);
    if has_more {
        items.pop();
    }
    let next_cursor = has_more
        .then(|| items.last().map(|r| r.started_at))
        .flatten();

    Ok(Json(TurnsListResponse { items, next_cursor }))
}

// ────────────────────────────────────────────────────────────────────────
// Logs & Metrics tab — prompt-versions list + restore
// (doc/logs_metrics_tab.md §4.1, §4.5)
// ────────────────────────────────────────────────────────────────────────

/// Hard cap on the prompt-versions list. Even pathological agents won't
/// edit prompts thousands of times, but a bound keeps the response sized
/// and avoids a runaway scan. CLAUDE.md §5.
const PROMPT_VERSIONS_PAGE_CAP: i64 = 100;

/// DB-shape row for `agent_prompt_versions`. Parsed into the domain
/// [`PromptVersionRow`] via TryFrom at the boundary (CLAUDE.md §1).
#[derive(Debug, sqlx::FromRow)]
struct PromptVersionRowDb {
    id: PromptVersionId,
    agent_id: AgentId,
    org_id: OrgId,
    version: PromptVersionNumber,
    system_prompt: String,
    model: Option<String>,
    edited_by: Option<UserId>,
    created_at: DateTime<Utc>,
}

impl TryFrom<PromptVersionRowDb> for PromptVersionRow {
    type Error = PromptVersionError;
    fn try_from(r: PromptVersionRowDb) -> Result<Self, Self::Error> {
        let parsed_prompt = AgentSystemPrompt::try_from(r.system_prompt)?;
        let parsed_model = r
            .model
            .map(|m| Model::try_from(m.as_str()))
            .transpose()
            .map_err(|_| {
                PromptVersionError::Parse(crate::types::ParseError::OutOfRange {
                    field: "model",
                    detail: "unknown model in agent_prompt_versions row",
                })
            })?;
        Ok(Self {
            id: r.id,
            agent_id: r.agent_id,
            org_id: r.org_id,
            version: r.version,
            system_prompt: parsed_prompt,
            model: parsed_model,
            edited_by: r.edited_by,
            created_at: r.created_at,
        })
    }
}

/// Wire shape for one prompt-version row in the list endpoint. Keeps
/// the route layer's external schema independent of the domain row.
#[derive(Debug, Serialize)]
struct PromptVersionWire {
    id: PromptVersionId,
    version: PromptVersionNumber,
    system_prompt: String,
    model: Option<&'static str>,
    edited_by: Option<UserId>,
    created_at: DateTime<Utc>,
}

impl From<PromptVersionRow> for PromptVersionWire {
    fn from(r: PromptVersionRow) -> Self {
        Self {
            id: r.id,
            version: r.version,
            system_prompt: r.system_prompt.as_str().to_owned(),
            model: r.model.map(Model::as_str),
            edited_by: r.edited_by,
            created_at: r.created_at,
        }
    }
}

#[derive(Debug, Serialize)]
struct PromptVersionsListResponse {
    items: Vec<PromptVersionWire>,
}

#[derive(Debug, Serialize)]
struct RestorePromptVersionResponse {
    /// The newly minted version number that overwrites the agent. Always
    /// `> source.version` because restore is append-only (doc §4.5).
    version: PromptVersionNumber,
    id: PromptVersionId,
    created_at: DateTime<Utc>,
}

/// `GET /agents/:id/prompt-versions` — every history row for one agent,
/// newest-first. Bounded by [`PROMPT_VERSIONS_PAGE_CAP`].
async fn list_agent_prompt_versions(
    State(state): State<AppState>,
    principal: Principal,
    Path(id): Path<Uuid>,
) -> Result<Json<PromptVersionsListResponse>, HttpError> {
    let agent_id = AgentId::from(id);
    if !visible_to(&state.pool, &principal, VisibilityTable::Agents, id).await? {
        return Err(HttpError::NotFound);
    }

    let mut tx = crate::auth::begin_as(&state.pool, &principal).await?;
    let rows = sqlx::query_as::<_, PromptVersionRowDb>(
        "SELECT id, agent_id, org_id, version, system_prompt, model, edited_by, created_at \
         FROM agent_prompt_versions \
         WHERE agent_id = $1 \
         ORDER BY version DESC \
         LIMIT $2",
    )
    .bind(agent_id)
    .bind(PROMPT_VERSIONS_PAGE_CAP)
    .fetch_all(&mut *tx)
    .await
    .map_err(AuthError::from)?;
    tx.commit().await.map_err(AuthError::from)?;

    let items = rows
        .into_iter()
        .map(|r| PromptVersionRow::try_from(r).map(PromptVersionWire::from))
        .collect::<Result<Vec<_>, PromptVersionError>>()?;
    Ok(Json(PromptVersionsListResponse { items }))
}

/// `POST /agents/:id/prompt-versions/:version/restore` — append-only
/// restore (doc §4.5). One transaction: lock agent → snapshot target →
/// next_version = max+1 → INSERT new row → UPDATE agents. History is
/// never rewritten; reverting v7 → v6 produces a byte-identical v8.
async fn restore_agent_prompt_version(
    State(state): State<AppState>,
    principal: Principal,
    Path((id, version_raw)): Path<(Uuid, i32)>,
) -> Result<Json<RestorePromptVersionResponse>, HttpError> {
    let agent_id = AgentId::from(id);
    let target_version = PromptVersionNumber::try_from(version_raw)
        .map_err(|e| HttpError::BadRequest(e.to_string()))?;

    if !visible_to(&state.pool, &principal, VisibilityTable::Agents, id).await? {
        return Err(HttpError::NotFound);
    }

    let now = state.clock.now_utc();
    let outcome = run_privileged(&state.pool, async |tx| {
        restore_in_tx(tx, agent_id, target_version, principal.user_id, now).await
    })
    .await?;
    Ok(Json(outcome))
}

async fn restore_in_tx(
    tx: &mut crate::auth::PrivilegedTx<'_>,
    agent_id: AgentId,
    target_version: PromptVersionNumber,
    acting_user: UserId,
    now: DateTime<Utc>,
) -> Result<RestorePromptVersionResponse, PromptVersionError> {
    // Lock the parent agent so a concurrent PATCH bumper or restore
    // serialises behind us.
    let agent_org: Option<(OrgId,)> =
        sqlx::query_as("SELECT org_id FROM agents WHERE id = $1 FOR UPDATE")
            .bind(agent_id)
            .fetch_optional(&mut **tx)
            .await?;
    let (org_id,) = agent_org.ok_or(PromptVersionError::AgentNotFound(agent_id))?;

    let snapshot: Option<(String, Option<String>)> = sqlx::query_as(
        "SELECT system_prompt, model FROM agent_prompt_versions \
         WHERE agent_id = $1 AND version = $2",
    )
    .bind(agent_id)
    .bind(target_version)
    .fetch_optional(&mut **tx)
    .await?;
    let (snapshot_prompt, snapshot_model) =
        snapshot.ok_or(PromptVersionError::VersionNotFound {
            agent: agent_id,
            version: target_version,
        })?;

    let max_version: Option<i32> =
        sqlx::query_scalar("SELECT MAX(version) FROM agent_prompt_versions WHERE agent_id = $1")
            .bind(agent_id)
            .fetch_one(&mut **tx)
            .await?;
    let next_number = match max_version {
        Some(n) => PromptVersionNumber::try_from(n.saturating_add(1))?,
        None => PromptVersionNumber::FIRST,
    };

    let new_id = PromptVersionId::new();
    sqlx::query(
        "INSERT INTO agent_prompt_versions \
         (id, agent_id, org_id, version, system_prompt, model, edited_by, created_at) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
    )
    .bind(new_id)
    .bind(agent_id)
    .bind(org_id)
    .bind(next_number)
    .bind(&snapshot_prompt)
    .bind(snapshot_model.as_deref())
    .bind(acting_user)
    .bind(now)
    .execute(&mut **tx)
    .await?;

    // Mirror onto the live `agents` row so workers pick up the change
    // after the AGENT_PROMPT_CACHE_TTL (60s).
    sqlx::query(
        "UPDATE agents \
         SET system_prompt = $2, model = $3, updated_at = $4 \
         WHERE id = $1",
    )
    .bind(agent_id)
    .bind(&snapshot_prompt)
    .bind(snapshot_model.as_deref())
    .bind(now)
    .execute(&mut **tx)
    .await?;

    Ok(RestorePromptVersionResponse {
        version: next_number,
        id: new_id,
        created_at: now,
    })
}
