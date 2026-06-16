//! Prompt and request endpoints:
//! * `POST /prompts` — post a human message into a thread (tags drive triggers)
//! * `POST /requests/{id}/cancel` — request cancellation
//!
//! In the Slack-parity model a prompt is a human's posted message. Tags are
//! explicit (`tags: [{kind, id}]`, parsed by the FE which owns the roster —
//! the BE never scans message text):
//!
//! * **zero tags** — a plain post. The row lands in the feed; no agent runs,
//!   no trigger row exists (`request_id: null` in the response).
//! * **agent tag** — one trigger + one fresh DAG per tagged agent
//!   (`@X @Y` = two DAGs). Idempotent per `(key, agent)`.
//! * **human tag** — stored as the row's receiver / rendered as a mention;
//!   no trigger.
//!
//! The first POST (no `thread_id`) creates the thread — in a channel when
//! `channel_id` is given (member-gated), else a DM, which requires
//! `counterpart` (the colleague the conversation is with — human or agent).
//! An untagged message in a DM whose counterpart is an agent still triggers
//! that agent (the conversation is addressed to them, Slack-style); channel
//! messages trigger only on explicit tags.
//!
//! Retries dedupe on the posted row's `idempotency_key` (an untagged post has
//! no trigger row to dedupe on) and the per-agent trigger keys, so a retry of
//! a partially-failed submit converges instead of double-posting.

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
use crate::colleagues::ColleagueId;
use crate::provider::limits::MAX_ATTACHMENTS_PER_MESSAGE;
use crate::provider::{Attachment, ChatMessage, RawAttachment, UserContent};
use crate::runtime::{
    IdempotencyKey, NewTrigger, PromptRequestId, RequestKindPayload, RequestStatus,
};
use crate::threads::{
    MAX_TAGS_PER_MESSAGE, MessageKind, NewMessage, PgThreadStore, ThreadId, ThreadMessageId,
    ThreadStore,
};
use crate::types::Prompt;

use super::super::error::HttpError;
use super::super::state::AppState;

/// A tag target, resolved from the wire by the HTTP handler (or constructed
/// directly by Principal-free callers like the OAuth resume).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum TagTarget {
    Agent(AgentId),
    Human(UserId),
}

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
    /// post when `channel_id` is set, else a DM with `counterpart`).
    pub thread_id: Option<ThreadId>,
    /// Explicit @tags, in message order. Agents trigger; humans render.
    pub tags: Vec<TagTarget>,
    /// The text body. `None` for an attachment-only message (issue #187) —
    /// at least one of `content` / `attachments` must be present.
    pub content: Option<Prompt>,
    /// Image/file attachments accompanying the text, in display order
    /// (issue #187). Empty for a plain text prompt.
    pub attachments: Vec<Attachment>,
    pub idempotency_key: IdempotencyKey,
    /// Post the new thread into this channel. Honored only when `thread_id`
    /// is `None` (a new root); a channel on a continuation is meaningless
    /// because location is inherited from the thread.
    pub channel_id: Option<ChannelId>,
    /// The colleague a new DM root is with. Required when both `thread_id`
    /// and `channel_id` are absent; ignored otherwise.
    pub counterpart: Option<TagTarget>,
}

/// Outcome of [`submit_internal`].
#[derive(Debug, Clone)]
pub(super) struct SubmitOutcome {
    /// First enqueued trigger, `None` when no agent was tagged (plain post).
    pub request_id: Option<PromptRequestId>,
    pub thread_id: ThreadId,
    /// `None` when no trigger was enqueued.
    pub status: Option<RequestStatus>,
    /// Every agent this message woke, in tag order (implicit DM counterpart
    /// last). The FE keys its "thinking…" placeholder on this.
    pub triggered_agent_ids: Vec<AgentId>,
    /// `true` when the `idempotency_key`'s posted row already existed (a
    /// retry); the HTTP handler maps it to `200 OK` instead of `202`.
    pub existed: bool,
}

/// A tag resolved to its colleague row. `agent` is `Some` for agent tags —
/// the ones that enqueue triggers.
#[derive(Debug, Clone, Copy)]
struct ResolvedTag {
    colleague: ColleagueId,
    agent: Option<AgentId>,
}

/// Build the user message body: the optional text block followed by one
/// image/file block per attachment, classified by mime (issue #187). Text
/// leads so the prompt reads naturally before its attachments on every
/// provider. At least one of `content` / `attachments` must be present.
fn user_content(content: Option<&Prompt>, attachments: &[Attachment]) -> Vec<UserContent> {
    let mut out = Vec::with_capacity(usize::from(content.is_some()) + attachments.len());
    if let Some(c) = content {
        out.push(UserContent::Text(c.as_str().to_owned()));
    }
    for att in attachments {
        out.push(if att.mime().is_image() {
            UserContent::Image(att.clone())
        } else {
            UserContent::File(att.clone())
        });
    }
    assert!(
        !out.is_empty(),
        "invariant: handler requires text or attachments"
    );
    out
}

/// Principal-free prompt submission. The public `POST /prompts` handler
/// extracts a `Principal` and delegates here; the OAuth callback (which
/// has no cookie session) constructs `SubmitPromptParams` directly.
pub(super) async fn submit_internal(
    state: &AppState,
    params: SubmitPromptParams,
) -> Result<SubmitOutcome, HttpError> {
    assert!(
        params.tags.len() <= MAX_TAGS_PER_MESSAGE,
        "invariant: handler caps tags before submit_internal"
    );
    let caller = Caller::new(params.user_id, params.org_id);
    let store = PgThreadStore::new(state.pool.clone(), state.clock.clone());

    // Three independent reads, overlapped: the retry lookup (a retry reuses
    // the original posted row — no duplicate message — but still re-runs the
    // per-(key,agent) trigger enqueues so a partial failure heals), the
    // sender colleague, and the explicit tags.
    let (existing, sender, mut tags) = tokio::try_join!(
        find_existing_post(
            state,
            params.user_id,
            params.org_id,
            &params.idempotency_key
        ),
        async {
            state
                .colleagues
                .resolve_user(params.org_id, params.user_id)
                .await
                .map_err(crate::http::HttpError::from)
        },
        resolve_tags(state, params.org_id, &params.tags),
    )?;
    let existed = existing.is_some();

    // Fold in the implicit DM receiver: an *untagged* message in a DM whose
    // counterpart is an agent wakes that agent (the conversation is with
    // them). An explicit agent tag overrides the implicit routing — tagging
    // agent B inside a DM with agent A wakes B only, and the next untagged
    // message returns to A (never `last_agent`).
    if !tags.iter().any(|t| t.agent.is_some())
        && let Some(implicit) =
            implicit_dm_receiver(state, &store, params.org_id, &params, existing).await?
    {
        tags.push(implicit);
    }
    let agent_tags: Vec<(ColleagueId, AgentId)> = tags
        .iter()
        .filter_map(|t| t.agent.map(|a| (t.colleague, a)))
        .collect();

    // Admission gate BEFORE any write: a trigger-bearing submit over the
    // monthly cap gets an immediate 429 with nothing persisted — no thread,
    // no posted row, no trigger. A plain post costs nothing and never 429s.
    if !agent_tags.is_empty() {
        state
            .billing
            .check_or_fail_for_user(params.user_id, params.org_id)
            .await?;
    }

    // Resolve the thread + the posted row in one shot. A retry reuses both
    // from the original row (no duplicate append); a fresh submit resolves
    // the thread (a continuation is visibility-gated; a new root creates a
    // member-gated channel post or a counterpart-required DM) then appends.
    let (thread, trigger_msg) = if let Some(pair) = existing {
        pair
    } else {
        let thread = resolve_thread(state, &store, &caller, &params, sender).await?;
        let msg = store
            .append(
                &caller,
                thread,
                NewMessage {
                    kind: MessageKind::Posted,
                    sender: Some(sender),
                    owner_agent_id: None,
                    receiver: tags.first().map(|t| t.colleague),
                    body: ChatMessage::User(user_content(
                        params.content.as_ref(),
                        &params.attachments,
                    )),
                    request_id: None,
                    idempotency_key: Some(params.idempotency_key.clone()),
                },
            )
            .await
            .map_err(thread_err)?;
        (thread, msg)
    };

    if agent_tags.is_empty() {
        return Ok(SubmitOutcome {
            request_id: None,
            thread_id: thread,
            status: None,
            triggered_agent_ids: Vec::new(),
            existed,
        });
    }

    let (request_ids, triggered) = enqueue_tag_triggers(
        state,
        &store,
        &caller,
        &params,
        sender,
        thread,
        trigger_msg,
        &agent_tags,
    )
    .await?;

    Ok(SubmitOutcome {
        request_id: request_ids.first().copied(),
        thread_id: thread,
        status: Some(RequestStatus::Pending),
        triggered_agent_ids: triggered,
        existed,
    })
}

/// One trigger + one fresh DAG per tagged agent (`@X @Y` = two DAGs), each
/// idempotent per `(client_key, agent)` so retries converge.
#[allow(clippy::too_many_arguments)]
async fn enqueue_tag_triggers(
    state: &AppState,
    store: &PgThreadStore,
    caller: &Caller,
    params: &SubmitPromptParams,
    sender: ColleagueId,
    thread: ThreadId,
    trigger_msg: ThreadMessageId,
    agent_tags: &[(ColleagueId, AgentId)],
) -> Result<(Vec<PromptRequestId>, Vec<AgentId>), HttpError> {
    assert!(
        agent_tags.len() <= MAX_TAGS_PER_MESSAGE + 1,
        "invariant: explicit tags are capped; at most one implicit receiver joins"
    );
    let mut request_ids = Vec::with_capacity(agent_tags.len());
    let mut triggered = Vec::with_capacity(agent_tags.len());
    for (_, agent_id) in agent_tags {
        let state_id = store
            .resolve_participation(caller, thread, *agent_id)
            .await
            .map_err(thread_err)?;
        let key = per_agent_key(&params.idempotency_key, *agent_id)?;
        let request_id = state
            .queue
            .enqueue_trigger(NewTrigger {
                org_id: params.org_id,
                acting_user_id: params.user_id,
                thread_id: Some(thread),
                state_id: Some(state_id),
                background_turn_id: None,
                sender_colleague_id: sender,
                receiver_agent_id: *agent_id,
                root_request_id: None,
                trigger_message_id: Some(trigger_msg),
                idempotency_key: key,
                kind_payload: RequestKindPayload::Normal {},
            })
            .await?;
        request_ids.push(request_id);
        triggered.push(*agent_id);
    }
    Ok((request_ids, triggered))
}

/// Resolve (or create) the thread a submit lands in.
async fn resolve_thread(
    state: &AppState,
    store: &PgThreadStore,
    caller: &Caller,
    params: &SubmitPromptParams,
    sender: ColleagueId,
) -> Result<ThreadId, HttpError> {
    if let Some(thread) = params.thread_id {
        // Continuation: gate on visibility (channel membership / DM pair) —
        // RLS alone gates org membership, not channel membership.
        let visible = store.visible_to(caller, thread).await.map_err(thread_err)?;
        if !visible {
            return Err(HttpError::NotFound);
        }
        return Ok(thread);
    }
    if let Some(channel) = params.channel_id {
        ensure_channel_member(state, caller.user_id, caller.org_id, channel).await?;
        let thread = store
            .create_thread(caller, Some(channel), None, sender, None)
            .await
            .map_err(thread_err)?;
        return Ok(thread);
    }
    // Fresh DM root: the counterpart names who the conversation is with.
    let target = params.counterpart.ok_or_else(|| {
        HttpError::BadRequest("a direct message requires a counterpart".to_owned())
    })?;
    let counterpart = resolve_target_colleague(state, caller.org_id, target).await?;
    let thread = store
        .create_thread(caller, None, None, sender, Some(counterpart.colleague))
        .await
        .map_err(thread_err)?;
    Ok(thread)
}

/// The implicit DM receiver of an *untagged* message: the DM's agent
/// counterpart. For a fresh root the counterpart target is on the params; a
/// continuation (or a retry) reads it off the thread row. Channel posts and
/// human-counterpart DMs return `None`.
async fn implicit_dm_receiver(
    state: &AppState,
    store: &PgThreadStore,
    org: OrgId,
    params: &SubmitPromptParams,
    existing: Option<(ThreadId, ThreadMessageId)>,
) -> Result<Option<ResolvedTag>, HttpError> {
    // Which thread (if any) already pins the conversation: a retry's posted
    // row wins, else an explicit continuation. A fresh root has no thread yet.
    let thread = existing.map(|(t, _)| t).or(params.thread_id);
    let Some(thread) = thread else {
        // Fresh root: a channel post has no implicit receiver; a DM root's
        // counterpart target is on the params (validated in resolve_thread).
        return match params.counterpart {
            Some(target @ TagTarget::Agent(_)) if params.channel_id.is_none() => {
                Ok(Some(resolve_target_colleague(state, org, target).await?))
            }
            _ => Ok(None),
        };
    };
    // A missing thread yields no implicit receiver here; `resolve_thread`'s
    // visibility gate downstream turns it into the proper 404.
    let counterpart = match store.dm_counterpart(thread).await {
        Ok(counterpart) => counterpart,
        Err(crate::threads::ThreadError::NotFound(_)) => None,
        Err(e) => return Err(thread_err(e)),
    };
    let Some(counterpart) = counterpart else {
        return Ok(None);
    };
    let colleague = state
        .colleagues
        .read(counterpart)
        .await
        .map_err(crate::http::HttpError::from)?;
    Ok(colleague.agent_id().map(|agent| ResolvedTag {
        colleague: counterpart,
        agent: Some(agent),
    }))
}

/// Resolve the wire tags to colleague rows, in order.
async fn resolve_tags(
    state: &AppState,
    org: OrgId,
    tags: &[TagTarget],
) -> Result<Vec<ResolvedTag>, HttpError> {
    assert!(
        tags.len() <= MAX_TAGS_PER_MESSAGE,
        "invariant: capped above"
    );
    let mut out = Vec::with_capacity(tags.len());
    for tag in tags {
        out.push(resolve_target_colleague(state, org, *tag).await?);
    }
    Ok(out)
}

/// Resolve one [`TagTarget`] to its colleague row within `org`. An unknown
/// satellite (agent/user not in the org) surfaces as 400 — the FE roster was
/// stale, not a server fault.
async fn resolve_target_colleague(
    state: &AppState,
    org: OrgId,
    target: TagTarget,
) -> Result<ResolvedTag, HttpError> {
    match target {
        TagTarget::Agent(agent_id) => {
            let colleague = state
                .colleagues
                .resolve_agent(org, agent_id)
                .await
                .map_err(|_| HttpError::BadRequest(format!("unknown agent tag {agent_id}")))?;
            Ok(ResolvedTag {
                colleague,
                agent: Some(agent_id),
            })
        }
        TagTarget::Human(user_id) => {
            let colleague = state
                .colleagues
                .resolve_user(org, user_id)
                .await
                .map_err(|_| HttpError::BadRequest(format!("unknown human tag {user_id}")))?;
            Ok(ResolvedTag {
                colleague,
                agent: None,
            })
        }
    }
}

/// Per-agent trigger idempotency: the client key suffixed with the agent id,
/// so `@X @Y` retries dedupe per agent. Rejects a client key long enough to
/// push the suffixed form past the shared cap.
fn per_agent_key(key: &IdempotencyKey, agent: AgentId) -> Result<IdempotencyKey, HttpError> {
    IdempotencyKey::try_from(format!("{}:{}", key.as_str(), agent.as_uuid()))
        .map_err(|e| HttpError::BadRequest(format!("idempotency_key too long for tags: {e}")))
}

/// Map a thread-store failure to an HTTP status. Membership/visibility is
/// pre-validated, so a fault here is an internal error.
fn thread_err(e: crate::threads::ThreadError) -> HttpError {
    tracing::error!(error = %e, "prompts.thread.store.error");
    HttpError::Internal
}

/// Look up the posted row already appended under `idempotency_key` (a retry).
/// Tenant-scoped. Returns `(thread_id, message_id)` so the submit reuses the
/// original row.
async fn find_existing_post(
    state: &AppState,
    user: UserId,
    org: OrgId,
    key: &IdempotencyKey,
) -> Result<Option<(ThreadId, ThreadMessageId)>, HttpError> {
    let mut tx = begin_as_user(&state.pool, user)
        .await
        .map_err(AuthError::from)?;
    let row: Option<(ThreadId, ThreadMessageId)> = sqlx::query_as(
        "SELECT thread_id, id FROM thread_messages \
         WHERE org_id = $1 AND idempotency_key = $2",
    )
    .bind(org)
    .bind(key.as_str())
    .fetch_optional(&mut *tx)
    .await
    .map_err(AuthError::from)?;
    tx.commit().await.map_err(AuthError::from)?;
    Ok(row)
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

/// One wire-side tag: `{kind: "agent"|"human", id: <satellite uuid>}` —
/// `id` is an `agents.id` or `users.id`, the ids the FE already holds from
/// `GET /agents` and the channel-member roster. Colleague resolution happens
/// server-side.
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum TagRefWire {
    Agent { id: AgentId },
    Human { id: UserId },
}

impl From<TagRefWire> for TagTarget {
    fn from(t: TagRefWire) -> Self {
        match t {
            TagRefWire::Agent { id } => Self::Agent(id),
            TagRefWire::Human { id } => Self::Human(id),
        }
    }
}

#[derive(Debug, Deserialize)]
struct SubmitPromptRequest {
    /// Continuing an existing thread — omit for the first prompt.
    #[serde(default)]
    thread_id: Option<Uuid>,
    /// Explicit @tags in message order. Empty = a plain post.
    #[serde(default)]
    tags: Vec<TagRefWire>,
    /// Post into this channel (a new thread). Ignored when `thread_id` is
    /// `Some` (a continuation inherits its thread's location).
    #[serde(default)]
    channel_id: Option<Uuid>,
    /// Who a fresh DM root is with. Required when neither `thread_id` nor
    /// `channel_id` is given.
    #[serde(default)]
    counterpart: Option<TagRefWire>,
    content: String,
    /// Image/file attachment references (issue #187). Each was uploaded via
    /// `POST /uploads/attachment`, which returned `{url, mime, filename, size}`.
    #[serde(default)]
    attachments: Vec<RawAttachment>,
    idempotency_key: String,
}

#[derive(Debug, Serialize)]
struct SubmitPromptResponse {
    /// First enqueued trigger; `null` for a plain (untagged) post.
    request_id: Option<PromptRequestId>,
    thread_id: ThreadId,
    /// `null` when no agent was triggered.
    status: Option<RequestStatus>,
    /// Agents this message woke — drives the FE's per-agent "thinking…".
    triggered_agent_ids: Vec<AgentId>,
}

async fn submit_prompt(
    State(state): State<AppState>,
    principal: Principal,
    Json(payload): Json<SubmitPromptRequest>,
) -> Result<(StatusCode, Json<SubmitPromptResponse>), HttpError> {
    let idempotency_key = IdempotencyKey::try_from(payload.idempotency_key)
        .map_err(|e| HttpError::BadRequest(e.to_string()))?;
    if payload.tags.len() > MAX_TAGS_PER_MESSAGE {
        return Err(HttpError::BadRequest(format!(
            "too many tags: max {MAX_TAGS_PER_MESSAGE}, got {}",
            payload.tags.len()
        )));
    }
    if payload.attachments.len() > MAX_ATTACHMENTS_PER_MESSAGE {
        return Err(HttpError::BadRequest(format!(
            "too many attachments: max {MAX_ATTACHMENTS_PER_MESSAGE}, got {}",
            payload.attachments.len()
        )));
    }
    // Parse each attachment reference at the boundary (CLAUDE.md §1): a bad
    // url/mime/filename/size is rejected here, before any write.
    let attachments = payload
        .attachments
        .into_iter()
        .map(Attachment::try_from)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| HttpError::BadRequest(e.to_string()))?;

    // Text is optional when at least one attachment is present (issue #187);
    // otherwise an empty body is rejected by the `Prompt` smart constructor.
    let content = if payload.content.trim().is_empty() && !attachments.is_empty() {
        None
    } else {
        Some(Prompt::try_from(payload.content).map_err(|e| HttpError::BadRequest(e.to_string()))?)
    };

    let outcome = submit_internal(
        &state,
        SubmitPromptParams {
            user_id: principal.user_id,
            org_id: principal.active_org_id,
            thread_id: payload.thread_id.map(ThreadId::from),
            tags: payload.tags.into_iter().map(TagTarget::from).collect(),
            content,
            attachments,
            idempotency_key,
            channel_id: payload.channel_id.map(ChannelId::from),
            counterpart: payload.counterpart.map(TagTarget::from),
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
            triggered_agent_ids: outcome.triggered_agent_ids,
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

#[cfg(test)]
mod tests {
    use super::*;

    fn att(mime: &str, name: &str) -> Attachment {
        Attachment::try_from(RawAttachment {
            url: "https://assets.example/attachments/x.bin".to_owned(),
            mime: mime.to_owned(),
            filename: name.to_owned(),
            size: 64,
        })
        .expect("valid")
    }

    #[test]
    fn user_content_text_only_is_single_text_block() {
        let prompt = Prompt::try_from("hello").expect("non-empty");
        let blocks = user_content(Some(&prompt), &[]);
        assert_eq!(blocks.len(), 1);
        assert!(matches!(blocks[0], UserContent::Text(_)));
    }

    #[test]
    fn user_content_classifies_attachments_by_mime() {
        let prompt = Prompt::try_from("see attached").expect("non-empty");
        let atts = vec![
            att("image/png", "a.png"),
            att("application/pdf", "b.pdf"),
            att(
                "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
                "c.xlsx",
            ),
        ];
        let blocks = user_content(Some(&prompt), &atts);
        assert_eq!(blocks.len(), 4);
        assert!(matches!(blocks[0], UserContent::Text(_)));
        assert!(matches!(blocks[1], UserContent::Image(_)));
        assert!(matches!(blocks[2], UserContent::File(_)));
        assert!(matches!(blocks[3], UserContent::File(_)));
    }

    #[test]
    fn user_content_attachment_only_omits_text_block() {
        // Issue #187: an image with no caption is a single Image block, no
        // empty Text block ahead of it.
        let blocks = user_content(None, &[att("image/png", "a.png")]);
        assert_eq!(blocks.len(), 1);
        assert!(matches!(blocks[0], UserContent::Image(_)));
    }
}
