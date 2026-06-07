//! Operator audit + write surface for the agent memory subsystem
//! (doc/memory.md §1.9).
//!
//! All paths land under `/agents/{id}/memory*`. Writes go through the
//! same journal that the agent's tool calls take, but with
//! `MutationSource::Operator` so pinned-row protection is bypassed and
//! validation events fire on `validated`/`core` notes.

use axum::Json;
use axum::Router;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use uuid::Uuid;

use crate::agents::AgentId;
use crate::auth::Principal;
use crate::memory::{
    MemoryContent, MemoryEventId, MemoryEventPayload, MemoryId, MemoryKind, MemoryMutation,
    MemoryRow, MemoryState, MutationKind, MutationSource, MutationSourceKind, ValidationOrigin,
};

use super::super::error::HttpError;
use super::super::state::AppState;

/// Tenant gate for every memory route: open a `begin_as` tx as the
/// principal and confirm the path-scoped `agent_id` is visible under
/// RLS. A cross-org id resolves to `None` (RLS hides it) and surfaces
/// as 404 — the same shape as "this agent doesn't exist", which is the
/// right behaviour because we don't leak the existence of cross-org
/// agents.
///
/// All five memory tables are RLS-forced via migration 17, but the
/// memory store opens its own privileged tx and so wouldn't enforce
/// the visibility check on its own. Gating here keeps the route a
/// thin shell over the store without duplicating the membership rule.
async fn gate_agent(pool: &PgPool, principal: &Principal, agent: AgentId) -> Result<(), HttpError> {
    if !crate::auth::visible_to(
        pool,
        principal,
        crate::auth::VisibilityTable::Agents,
        agent.as_uuid(),
    )
    .await?
    {
        return Err(HttpError::NotFound);
    }
    Ok(())
}

pub(super) fn router() -> Router<AppState> {
    Router::new()
        .route(
            "/agents/{id}/memory",
            get(list_memory).post(create_memory_note),
        )
        .route("/agents/{id}/memory/events", get(list_events))
        .route("/agents/{id}/memory/{memory}/pin", post(pin_memory))
        .route("/agents/{id}/memory/{memory}/unpin", post(unpin_memory))
        .route(
            "/agents/{id}/memory/events/{event}/revert",
            post(revert_event),
        )
}

#[derive(Debug, Serialize)]
struct MemoryRowResponse {
    id: MemoryId,
    agent_id: AgentId,
    kind: String,
    content: String,
    state: String,
    pinned: bool,
    /// The agent turn that produced this row. `None` for operator notes
    /// and librarian-merged rows — the FE uses presence to decide whether
    /// to render a "source turn" link back into the chat view.
    source_turn_id: Option<crate::runtime::PromptRequestId>,
    created_at: DateTime<Utc>,
    last_validated_at: DateTime<Utc>,
    last_accessed_at: DateTime<Utc>,
    access_count: u64,
}

impl From<MemoryRow> for MemoryRowResponse {
    fn from(r: MemoryRow) -> Self {
        Self {
            id: r.id,
            agent_id: r.agent_id,
            kind: r.kind.as_str().to_owned(),
            content: r.content.as_str().to_owned(),
            state: r.state.as_str().to_owned(),
            pinned: r.pinned,
            source_turn_id: r.source_turn_id,
            created_at: r.created_at,
            last_validated_at: r.last_validated_at,
            last_accessed_at: r.last_accessed_at,
            access_count: r.access_count,
        }
    }
}

async fn list_memory(
    State(state): State<AppState>,
    principal: Principal,
    Path(id): Path<Uuid>,
) -> Result<Json<Vec<MemoryRowResponse>>, HttpError> {
    let agent = AgentId::from(id);
    gate_agent(&state.pool, &principal, agent).await?;
    let rows = state
        .memory_store
        .list(agent)
        .await
        .map_err(|e| HttpError::BadRequest(e.to_string()))?;
    Ok(Json(
        rows.into_iter().map(MemoryRowResponse::from).collect(),
    ))
}

#[derive(Debug, Deserialize)]
struct CreateNoteRequest {
    kind: MemoryKind,
    content: String,
    /// Default `held` — operator notes start at higher trust than an
    /// agent-written `tentative`.
    #[serde(default)]
    state: Option<MemoryState>,
    #[serde(default)]
    pinned: bool,
    /// The colleague a `collaborator` note is about. Required for
    /// `collaborator`, omitted otherwise (the store enforces consistency).
    #[serde(default)]
    subject: Option<crate::colleagues::ColleagueId>,
}

async fn create_memory_note(
    State(state): State<AppState>,
    principal: Principal,
    Path(id): Path<Uuid>,
    Json(payload): Json<CreateNoteRequest>,
) -> Result<(StatusCode, Json<MemoryRowResponse>), HttpError> {
    let agent = AgentId::from(id);
    gate_agent(&state.pool, &principal, agent).await?;
    let content = MemoryContent::try_from(payload.content).map_err(HttpError::Parse)?;
    let chosen_state = payload.state.unwrap_or(MemoryState::Held);
    let outcome = state
        .memory_store
        .apply(MemoryMutation::Write {
            agent,
            kind: payload.kind,
            content,
            state: chosen_state,
            pinned: payload.pinned,
            subject: payload.subject,
            source: MutationSource::Operator,
        })
        .await
        .map_err(|e| HttpError::BadRequest(e.to_string()))?;

    // If the operator endorsed the note at validated/core, also record a
    // validation event so the lifecycle log reflects the independent
    // signal.
    if matches!(chosen_state, MemoryState::Validated | MemoryState::Core)
        && let Err(e) = state
            .memory_store
            .record_validation(
                agent,
                outcome.memory_id,
                ValidationOrigin::Operator,
                Some("operator note"),
            )
            .await
    {
        tracing::warn!(error = %e, "operator.note.validation.error");
    }

    let row = outcome
        .row
        .ok_or_else(|| HttpError::BadRequest("write returned no row".into()))?;
    Ok((StatusCode::CREATED, Json(row.into())))
}

#[derive(Debug, Serialize)]
struct EventResponse {
    id: MemoryEventId,
    agent_id: AgentId,
    mutation: String,
    target_memory_id: MemoryId,
    content_before: Option<String>,
    content_after: Option<String>,
    source: String,
    source_turn_id: Option<crate::runtime::PromptRequestId>,
    created_at: DateTime<Utc>,
}

#[derive(Debug, Deserialize)]
struct ListEventsQuery {
    /// Optional `?source=turn|operator|librarian`.
    #[serde(default)]
    source: Option<MutationSourceKind>,
    /// Optional `?mutation=write|update|forget`.
    #[serde(default)]
    mutation: Option<MutationKind>,
}

async fn list_events(
    State(state): State<AppState>,
    principal: Principal,
    Path(id): Path<Uuid>,
    Query(filter): Query<ListEventsQuery>,
) -> Result<Json<Vec<EventResponse>>, HttpError> {
    let agent = AgentId::from(id);
    gate_agent(&state.pool, &principal, agent).await?;
    let events = state
        .memory_store
        .list_events(agent)
        .await
        .map_err(|e| HttpError::BadRequest(e.to_string()))?;

    let filtered = events
        .into_iter()
        .filter(|e| {
            filter.source.is_none_or(|s| e.source.kind() == s)
                && filter.mutation.is_none_or(|m| e.mutation_kind() == m)
        })
        .map(EventResponse::from)
        .collect();

    Ok(Json(filtered))
}

impl From<crate::memory::MemoryEvent> for EventResponse {
    fn from(e: crate::memory::MemoryEvent) -> Self {
        let (content_before, content_after) = match &e.payload {
            MemoryEventPayload::Write { content, .. } => (None, Some(content.as_str().to_owned())),
            MemoryEventPayload::Update { before, after, .. } => (
                Some(before.as_str().to_owned()),
                Some(after.as_str().to_owned()),
            ),
            MemoryEventPayload::Forget { before } => (Some(before.as_str().to_owned()), None),
        };
        Self {
            id: e.id,
            agent_id: e.agent_id,
            mutation: e.mutation_kind().as_str().to_owned(),
            target_memory_id: e.target_memory_id,
            content_before,
            content_after,
            source: e.source.kind().as_str().to_owned(),
            source_turn_id: e.source.turn_id(),
            created_at: e.created_at,
        }
    }
}

async fn pin_memory(
    State(state): State<AppState>,
    principal: Principal,
    Path((id, memory)): Path<(Uuid, Uuid)>,
) -> Result<Json<MemoryRowResponse>, HttpError> {
    let agent = AgentId::from(id);
    gate_agent(&state.pool, &principal, agent).await?;
    let row = state
        .memory_store
        .set_pinned(agent, MemoryId::from(memory), true)
        .await
        .map_err(|e| HttpError::BadRequest(e.to_string()))?;
    Ok(Json(row.into()))
}

async fn unpin_memory(
    State(state): State<AppState>,
    principal: Principal,
    Path((id, memory)): Path<(Uuid, Uuid)>,
) -> Result<Json<MemoryRowResponse>, HttpError> {
    let agent = AgentId::from(id);
    gate_agent(&state.pool, &principal, agent).await?;
    let row = state
        .memory_store
        .set_pinned(agent, MemoryId::from(memory), false)
        .await
        .map_err(|e| HttpError::BadRequest(e.to_string()))?;
    Ok(Json(row.into()))
}

#[derive(Debug, Serialize)]
#[serde(untagged)]
enum RevertResponse {
    Row(MemoryRowResponse),
    Removed { removed: bool },
}

async fn revert_event(
    State(state): State<AppState>,
    principal: Principal,
    Path((id, event)): Path<(Uuid, Uuid)>,
) -> Result<Json<RevertResponse>, HttpError> {
    let agent = AgentId::from(id);
    gate_agent(&state.pool, &principal, agent).await?;
    let event_id = MemoryEventId::from(event);
    let row = state
        .memory_store
        .revert_event(agent, event_id)
        .await
        .map_err(|e| HttpError::BadRequest(e.to_string()))?;
    Ok(Json(row.map_or_else(
        || RevertResponse::Removed { removed: true },
        |r| RevertResponse::Row(r.into()),
    )))
}
