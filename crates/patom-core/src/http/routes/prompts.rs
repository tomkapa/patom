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
use crate::auth::{AuthError, OrgId, Principal, UserId, begin_as_user};
use crate::channels::ChannelId;
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
    /// Post the new thread into this channel. Honored only when
    /// `session_id` is `None` (a new root); a channel on a reply is
    /// meaningless because location is inherited from the root. `None` ⇒ a
    /// direct message with the agent, private to the caller.
    pub channel_id: Option<ChannelId>,
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
    // Admission gate: reject a new prompt up front when the org has spent its
    // monthly cap, so the user gets an immediate 429 instead of enqueuing work
    // that the per-turn gate would fail mid-flight. Tenant-scoped
    // (`begin_as_user`) so a caller can only ever read its own org's counter.
    state
        .budget
        .check_or_fail_for_user(params.user_id, params.org_id)
        .await?;

    let receiver_agent_id = match params.session_id {
        Some(session_id) => session_agent_participant(state, session_id).await?,
        None => match params.agent_id {
            Some(id) => id,
            None => state.agents.default_id_for(params.org_id).await?,
        },
    };

    // A channel is honored only on a new root; a channel on a reply is
    // meaningless (location is inherited from the root). Validate up front
    // that the caller may post here, so a non-member 403s before any work is
    // enqueued.
    let root_channel = match (params.session_id, params.channel_id) {
        (None, Some(channel)) => {
            ensure_channel_member(state, params.user_id, params.org_id, channel).await?;
            Some(channel)
        }
        _ => None,
    };

    // Resolve the human's colleague_id so the queue receives a
    // colleague-backed sender; pg_queue resolves the receiver agent's
    // colleague inline.
    let human_colleague = state
        .colleagues
        .resolve_user(params.org_id, params.user_id)
        .await
        .map_err(crate::http::HttpError::from)?;
    let outcome = state
        .queue
        .enqueue_for_user(
            params.user_id,
            NewPromptRequest::normal(
                params.session_id,
                Participant::human(human_colleague, params.user_id),
                receiver_agent_id,
                None,
                params.content,
                params.idempotency_key,
                params.org_id,
                params.user_id,
            ),
        )
        .await?;

    // Stamp the freshly-minted root with its channel. Only on a genuinely new
    // row (`Inserted`) — an idempotent retry (`Existing`) already carries the
    // channel from the original insert. For a `session_id = None` enqueue the
    // returned `request_id` is the DAG root (`pr.id = pr.root_request_id`).
    if let (Some(channel), EnqueueOutcome::Inserted { request_id, .. }) = (root_channel, &outcome) {
        stamp_root_channel(state, params.user_id, params.org_id, *request_id, channel).await?;
    }
    Ok(outcome)
}

/// Reject a channel post by a non-member, or to an archived / cross-org
/// channel. Pinned to `org` because RLS gates membership in any org.
async fn ensure_channel_member(
    state: &AppState,
    user: UserId,
    org: OrgId,
    channel: ChannelId,
) -> Result<(), HttpError> {
    let mut tx = begin_as_user(&state.pool, user)
        .await
        .map_err(AuthError::from)?;
    let allowed: bool = sqlx::query_scalar(
        "SELECT EXISTS(\
            SELECT 1 FROM channels c \
            JOIN channel_members cm ON cm.channel_id = c.id \
            WHERE c.id = $1 AND c.org_id = $2 AND c.archived_at IS NULL AND cm.user_id = $3)",
    )
    .bind(channel)
    .bind(org)
    .bind(user)
    .fetch_one(&mut *tx)
    .await
    .map_err(AuthError::from)?;
    tx.commit().await.map_err(AuthError::from)?;
    if allowed {
        return Ok(());
    }
    Err(HttpError::Forbidden("channel.not_member"))
}

/// Stamp a root `prompt_requests` row with its channel. Org-pinned `WHERE`
/// (defense-in-depth alongside the `prompt_requests` org-isolation policy).
async fn stamp_root_channel(
    state: &AppState,
    user: UserId,
    org: OrgId,
    root: PromptRequestId,
    channel: ChannelId,
) -> Result<(), HttpError> {
    let mut tx = begin_as_user(&state.pool, user)
        .await
        .map_err(AuthError::from)?;
    sqlx::query("UPDATE prompt_requests SET channel_id = $1 WHERE id = $2 AND org_id = $3")
        .bind(channel)
        .bind(root)
        .bind(org)
        .execute(&mut *tx)
        .await
        .map_err(AuthError::from)?;
    tx.commit().await.map_err(AuthError::from)?;
    Ok(())
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
    /// Post into this channel (a new thread). Omit for a direct message with
    /// the agent. Ignored when `session_id` is `Some` (a reply inherits its
    /// root's location).
    #[serde(default)]
    channel_id: Option<Uuid>,
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
            channel_id: payload.channel_id.map(ChannelId::from),
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
