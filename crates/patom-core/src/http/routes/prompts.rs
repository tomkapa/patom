//! Prompt and request endpoints:
//! * `POST /prompts` — submit a prompt into a thread (the @tag entry point)
//! * `POST /requests/{id}/cancel` — request cancellation
//!
//! In the thread-feed model a prompt is a human's posted message that @tags an
//! agent. The first POST (no `thread_id`) creates a thread — in a channel if
//! `channel_id` is given, otherwise a direct message with the agent; subsequent
//! POSTs pass the `thread_id` returned from the response. Each POST appends the
//! human's `posted` row to the feed, resolves the agent's participation, and
//! enqueues a fresh-DAG trigger (each human message is its own turn budget).
//!
//! Per-request SSE (`GET /requests/{id}/stream`) is gone — the chat UI uses the
//! per-thread stream at `GET /threads/{thread_id}/stream`. See
//! `doc/backend_plan.md` for the rationale.

use axum::Json;
use axum::Router;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::post;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::agents::AgentId;
use crate::auth::{AuthError, Caller, OrgId, Principal, UserId, begin_as_user};
use crate::channels::ChannelId;
use crate::provider::{ChatMessage, UserContent};
use crate::runtime::{
    IdempotencyKey, NewTrigger, PromptRequestId, RequestKindPayload, RequestStatus,
};
use crate::threads::{MessageKind, NewMessage, PgThreadStore, ThreadId, ThreadStore};
use crate::types::Prompt;

use super::super::error::HttpError;
use super::super::state::AppState;

/// Inputs to [`submit_internal`] — the Principal-free service helper
/// that drives prompt submission for callers without a cookie session.
///
/// Two such callers today:
///   * the public OAuth callback (`GET /mcp-oauth/callback`), which
///     enqueues the synthetic `"I've connected {name}. Please continue."`
///     resume prompt into the thread it captured;
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
    /// Continue an existing thread. `None` ⇒ create a fresh thread (a channel
    /// post when `channel_id` is set, else a DM with the agent).
    pub thread_id: Option<ThreadId>,
    /// Agent to @tag. Omit to bind to the org's seeded default agent.
    pub agent_id: Option<AgentId>,
    pub content: Prompt,
    pub idempotency_key: IdempotencyKey,
    /// Post the new thread into this channel. Honored only when `thread_id`
    /// is `None` (a new root); a channel on a continuation is meaningless
    /// because location is inherited from the thread. `None` ⇒ a direct
    /// message with the agent, private to the caller.
    pub channel_id: Option<ChannelId>,
}

/// Outcome of [`submit_internal`].
#[derive(Debug, Clone)]
pub(super) struct SubmitOutcome {
    pub request_id: PromptRequestId,
    pub thread_id: ThreadId,
    pub status: RequestStatus,
    /// `true` when the `idempotency_key` had already been submitted (a retry);
    /// the HTTP handler maps it to `200 OK` instead of `202 ACCEPTED`.
    pub existed: bool,
}

/// Principal-free prompt submission. The public `POST /prompts` handler
/// extracts a `Principal` and delegates here; the OAuth callback (which
/// has no cookie session) constructs `SubmitPromptParams` directly.
///
/// A new root (`thread_id = None`) creates the thread (validating channel
/// membership for a channel post); a continuation appends to the existing
/// thread. Either way the human's `posted` row is appended @tagging the agent,
/// the agent's participation is resolved, and a fresh-DAG trigger is enqueued
/// (each human message mints its own turn budget). Idempotent on
/// `idempotency_key`: a retry returns the original trigger + thread WITHOUT
/// re-posting the human row.
pub(super) async fn submit_internal(
    state: &AppState,
    params: SubmitPromptParams,
) -> Result<SubmitOutcome, HttpError> {
    // Admission gate: reject a new prompt up front when the org has spent its
    // monthly cap, so the user gets an immediate 429 instead of enqueuing work
    // that the per-turn gate would fail mid-flight. Tenant-scoped
    // (`begin_as_user`) so a caller can only ever read its own org's counter.
    state
        .budget
        .check_or_fail_for_user(params.user_id, params.org_id)
        .await?;

    // Idempotency: a retry of the same submit returns the original trigger +
    // thread without appending a duplicate human row (the append is not itself
    // keyed; the trigger is). Checked before any write.
    if let Some(existing) = find_existing_trigger(
        state,
        params.user_id,
        params.org_id,
        &params.idempotency_key,
    )
    .await?
    {
        return Ok(existing);
    }

    let caller = Caller::new(params.user_id, params.org_id);
    let store = PgThreadStore::new(state.pool.clone(), state.clock.clone());

    // Which agent to @tag: an explicit `agent_id` wins; otherwise a
    // continuation routes to the thread's current agent (DM continuity), and a
    // fresh root (or a thread with no agent yet) falls back to the seeded default.
    let agent_id = if let Some(id) = params.agent_id {
        id
    } else if let Some(thread) = params.thread_id
        && let Some(agent) = store.last_agent(thread).await.map_err(thread_err)?
    {
        agent
    } else {
        state.agents.default_id_for(params.org_id).await?
    };

    // Independent directory reads — resolve both colleagues concurrently.
    let (human_colleague, agent_colleague) = tokio::join!(
        state.colleagues.resolve_user(params.org_id, params.user_id),
        state.colleagues.resolve_agent(params.org_id, agent_id),
    );
    let human_colleague = human_colleague.map_err(crate::http::HttpError::from)?;
    let agent_colleague = agent_colleague.map_err(crate::http::HttpError::from)?;

    // New root creates the thread (validating channel membership for a channel
    // post); a continuation reuses the existing thread.
    let thread = if let Some(thread) = params.thread_id {
        thread
    } else {
        if let Some(channel) = params.channel_id {
            ensure_channel_member(state, params.user_id, params.org_id, channel).await?;
        }
        store
            .create_thread(&caller, params.channel_id, None, human_colleague)
            .await
            .map_err(thread_err)?
    };

    // Append the human's posted row, @tagging the agent.
    let trigger_msg = store
        .append(
            &caller,
            thread,
            NewMessage {
                kind: MessageKind::Posted,
                sender: Some(human_colleague),
                owner_agent_id: None,
                receiver: Some(agent_colleague),
                body: ChatMessage::User(vec![UserContent::Text(
                    params.content.as_str().to_owned(),
                )]),
                request_id: None,
            },
        )
        .await
        .map_err(thread_err)?;

    // Resolve the agent's participation (the chat claim_key), then mint a fresh
    // DAG trigger (root_request_id = None) pointing at the posted row.
    let state_id = store
        .resolve_participation(&caller, thread, agent_id)
        .await
        .map_err(thread_err)?;
    let request_id = state
        .queue
        .enqueue_trigger(NewTrigger {
            org_id: params.org_id,
            acting_user_id: params.user_id,
            thread_id: Some(thread),
            state_id: Some(state_id),
            background_turn_id: None,
            sender_colleague_id: human_colleague,
            receiver_agent_id: agent_id,
            root_request_id: None,
            trigger_message_id: Some(trigger_msg),
            idempotency_key: params.idempotency_key,
            kind_payload: RequestKindPayload::Normal {},
        })
        .await?;

    Ok(SubmitOutcome {
        request_id,
        thread_id: thread,
        status: RequestStatus::Pending,
        existed: false,
    })
}

/// Map a thread-store failure to an HTTP status. Channel membership is
/// pre-validated, so a fault here is an internal error.
fn thread_err(e: crate::threads::ThreadError) -> HttpError {
    tracing::error!(error = %e, "prompts.thread.store.error");
    HttpError::Internal
}

/// Look up a chat trigger already enqueued under `idempotency_key` (a retry).
/// Tenant-scoped. Returns the original `(request_id, thread_id, status)` so the
/// submit short-circuits without re-posting the human row.
async fn find_existing_trigger(
    state: &AppState,
    user: UserId,
    org: OrgId,
    key: &IdempotencyKey,
) -> Result<Option<SubmitOutcome>, HttpError> {
    let mut tx = begin_as_user(&state.pool, user)
        .await
        .map_err(AuthError::from)?;
    let row: Option<(PromptRequestId, ThreadId, RequestStatus)> = sqlx::query_as(
        "SELECT id, thread_id, status FROM prompt_requests \
         WHERE org_id = $1 AND idempotency_key = $2 AND thread_id IS NOT NULL",
    )
    .bind(org)
    .bind(key.as_str())
    .fetch_optional(&mut *tx)
    .await
    .map_err(AuthError::from)?;
    tx.commit().await.map_err(AuthError::from)?;
    Ok(row.map(|(request_id, thread_id, status)| SubmitOutcome {
        request_id,
        thread_id,
        status,
        existed: true,
    }))
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

pub(super) fn router() -> Router<AppState> {
    Router::new()
        .route("/prompts", post(submit_prompt))
        .route("/requests/{id}/cancel", post(cancel_request))
}

#[derive(Debug, Deserialize)]
struct SubmitPromptRequest {
    /// Continuing an existing thread — omit for the first prompt.
    #[serde(default)]
    thread_id: Option<Uuid>,
    /// Which agent to @tag. Omit to bind to the org's seeded default agent.
    #[serde(default)]
    agent_id: Option<AgentId>,
    /// Post into this channel (a new thread). Omit for a direct message with
    /// the agent. Ignored when `thread_id` is `Some` (a continuation inherits
    /// its thread's location).
    #[serde(default)]
    channel_id: Option<Uuid>,
    content: String,
    idempotency_key: String,
}

#[derive(Debug, Serialize)]
struct SubmitPromptResponse {
    request_id: PromptRequestId,
    thread_id: ThreadId,
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

    let outcome = submit_internal(
        &state,
        SubmitPromptParams {
            user_id: principal.user_id,
            org_id: principal.active_org_id,
            thread_id: payload.thread_id.map(ThreadId::from),
            agent_id: payload.agent_id,
            content,
            idempotency_key,
            channel_id: payload.channel_id.map(ChannelId::from),
        },
    )
    .await?;

    let status_code = if outcome.existed {
        StatusCode::OK
    } else {
        StatusCode::ACCEPTED
    };
    Ok((
        status_code,
        Json(SubmitPromptResponse {
            request_id: outcome.request_id,
            thread_id: outcome.thread_id,
            status: outcome.status,
        }),
    ))
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
