//! Prompt and request endpoints:
//! * `POST /prompts` — submit a prompt; creates a session lazily on first call
//! * `POST /requests/{id}/cancel` — request cancellation
//!
//! Sessions are created lazily by the queue: the first POST without a
//! `session_id` mints a new conversation; subsequent POSTs pass the
//! `session_id` returned from the response. There is no separate
//! `POST /sessions` — that intermediate step is gone with the multi-agent
//! schema (see `migrations/00000000000004_multi_agent_comm.up.sql`).
//!
//! Per-request SSE (`GET /requests/{id}/stream`) is gone — the chat UI uses
//! the DAG-wide stream at `GET /threads/{root}/stream`. See
//! `doc/backend_plan.md` for the rationale.

use axum::Json;
use axum::Router;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::post;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::agents::AgentId;
use crate::auth::{OrgId, Principal, UserId};
use crate::runtime::{
    EnqueueOutcome, IdempotencyKey, NewPromptRequest, PromptRequestId, RequestStatus,
};
use crate::session::SessionId;
use crate::types::{Participant, ParticipantKind, Prompt};

use super::super::error::HttpError;
use super::super::state::AppState;

/// Inputs to [`submit_internal`] — the Principal-free service helper
/// that drives prompt submission for callers without a cookie session.
///
/// Two such callers today:
///   * the public OAuth callback (`GET /mcp-oauth/callback`), which
///     enqueues the synthetic `"I've connected {name}. Please continue."`
///     resume prompt;
///   * future channel adapters (Lark, CLI, …) that need to drive a
///     resume without a Principal extractor.
///
/// `(user_id, org_id)` substitute for what `Principal` would have
/// supplied; callers source them from the consumed `mcp_oauth_pending`
/// row, which was authenticated when minted.
#[derive(Debug, Clone)]
pub(super) struct SubmitPromptParams {
    pub user_id: UserId,
    pub org_id: OrgId,
    /// Continue an existing conversation. Both `session_id` and
    /// `agent_id` must be supplied for a resume (the existing session
    /// already pins the agent — see `submit_internal` for the
    /// preservation rule).
    pub session_id: Option<SessionId>,
    pub agent_id: Option<AgentId>,
    pub content: Prompt,
    pub idempotency_key: IdempotencyKey,
}

/// Principal-free prompt submission. The public `POST /prompts` handler
/// extracts a `Principal` and delegates here; the OAuth callback (which
/// has no cookie session) constructs `SubmitPromptParams` directly.
///
/// Mirrors `submit_prompt`'s rules:
///   * Continuing an existing session preserves the session's agent
///     participant — any caller-supplied `agent_id` is ignored for a
///     non-`None` session.
///   * Fresh sessions consult the request payload, falling back to the
///     org's seeded default agent.
pub(super) async fn submit_internal(
    state: &AppState,
    params: SubmitPromptParams,
) -> Result<EnqueueOutcome, HttpError> {
    let receiver_agent_id = match params.session_id {
        Some(session_id) => session_agent_participant(state, session_id).await?,
        None => match params.agent_id {
            Some(id) => id,
            None => state.agents.default_id_for(params.org_id).await?,
        },
    };

    let outcome = state
        .queue
        .enqueue_for_user(
            params.user_id,
            NewPromptRequest::normal(
                params.session_id,
                Participant::Human,
                receiver_agent_id,
                None,
                params.content,
                params.idempotency_key,
                params.org_id,
                params.user_id,
            ),
        )
        .await?;
    Ok(outcome)
}

pub(super) fn router() -> Router<AppState> {
    Router::new()
        .route("/prompts", post(submit_prompt))
        .route("/requests/{id}/cancel", post(cancel_request))
}

#[derive(Debug, Deserialize)]
struct SubmitPromptRequest {
    /// Continuing an existing conversation — omit for the first prompt.
    #[serde(default)]
    session_id: Option<SessionId>,
    /// Which agent should handle this prompt. Omit to bind the new conversation
    /// to the seeded default agent. Ignored when `session_id` is `Some` —
    /// the existing session's receiver agent is preserved.
    #[serde(default)]
    agent_id: Option<AgentId>,
    content: String,
    idempotency_key: String,
}

#[derive(Debug, Serialize)]
struct SubmitPromptResponse {
    request_id: PromptRequestId,
    session_id: SessionId,
    status: RequestStatus,
}

async fn submit_prompt(
    State(state): State<AppState>,
    principal: Principal,
    Json(payload): Json<SubmitPromptRequest>,
) -> Result<(StatusCode, Json<SubmitPromptResponse>), HttpError> {
    let content =
        Prompt::try_from(payload.content).map_err(|e| HttpError::BadRequest(e.to_string()))?;
    let idempotency_key = IdempotencyKey::try_from(payload.idempotency_key)
        .map_err(|e| HttpError::BadRequest(e.to_string()))?;

    // Tenancy plumbing for the queue's implicit session-create path
    // (`NewPromptRequest::session = None`). For a continuing session the
    // queue won't mint a row, so the (org_id, user_id) we pass here is
    // only consulted on first prompt.
    let outcome = submit_internal(
        &state,
        SubmitPromptParams {
            user_id: principal.user_id,
            org_id: principal.active_org_id,
            session_id: payload.session_id,
            agent_id: payload.agent_id,
            content,
            idempotency_key,
        },
    )
    .await?;

    let status_code = match outcome {
        EnqueueOutcome::Inserted { .. } => StatusCode::ACCEPTED,
        EnqueueOutcome::Existing { .. } => StatusCode::OK,
    };
    Ok((
        status_code,
        Json(SubmitPromptResponse {
            request_id: outcome.request_id(),
            session_id: outcome.session(),
            status: outcome.status(),
        }),
    ))
}

/// Read the agent participant of `session_id`. Human-rooted DAGs always
/// have exactly one Human and one Agent participant; an unexpected pair
/// (Agent-Agent or Human-Human) would be a backend invariant violation
/// for a human-rooted thread, and surfaces as `Internal`.
async fn session_agent_participant(
    state: &AppState,
    session_id: SessionId,
) -> Result<AgentId, HttpError> {
    let (a, b) = state.sessions.participants(session_id).await?;
    match (a.kind(), b.kind()) {
        (ParticipantKind::Agent, _) => Ok(a.agent_id().expect("invariant: agent kind has id")),
        (_, ParticipantKind::Agent) => Ok(b.agent_id().expect("invariant: agent kind has id")),
        _ => Err(HttpError::Internal),
    }
}

async fn cancel_request(
    State(state): State<AppState>,
    principal: Principal,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, HttpError> {
    let request_id = PromptRequestId::from(id);
    // Tenant gate: 404 cross-org / unknown ids without leaking existence
    // before dispatching the privileged cancellation write.
    if !crate::auth::visible_to(
        &state.pool,
        &principal,
        crate::auth::VisibilityTable::PromptRequests,
        request_id.as_uuid(),
    )
    .await?
    {
        return Err(HttpError::NotFound);
    }
    state.queue.request_cancellation(request_id).await?;
    Ok(StatusCode::NO_CONTENT)
}
