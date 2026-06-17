//! `send_message` — the agent's SOLE output verb in the thread-feed model.
//!
//! See SPEC §3 + doc/thread-chat-refactor.md §2. Plain assistant text is a
//! private turn artifact; communication happens only through this tool, which
//! appends a `kind='posted'` row to the thread feed (the egress the worker's
//! ping-pong guard watches for).
//!
//! `receiver` is a single optional colleague id, all posting to the *current*
//! thread (`ctx.thread_id`). The recipient's kind (human vs agent) is derived
//! from the resolved `colleagues` row, not declared by the caller:
//!
//! - **none** (`receiver` omitted) — an untagged post (announcement / thinking
//!   aloud to the channel). Posts and returns.
//! - **human colleague** — agents reach a human only inside a thread the human
//!   can see: a channel thread requires channel membership
//!   (`ThreadStore::is_channel_member`), rejected with no auto-add; a DM thread
//!   is always reachable. Posts a `receiver`-addressed row.
//! - **agent colleague** — agents are org-global (reachable in any thread, no
//!   membership). Posts the row, resolves the receiver's `(thread, agent)`
//!   participation, bumps the DAG turn budget, and enqueues a `prompt_requests`
//!   *trigger* whose `trigger_message_id` is the posted row; the worker picks
//!   it up.
//!
//! Returns the thread id and (for agent receivers) the trigger request id.

use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tracing::{info, warn};

use crate::agents::AgentId;
use crate::auth::Caller;
use crate::channels::ChannelId;
use crate::colleagues::{ColleagueError, ColleagueId, SharedColleagueStore};
use crate::observability::log::preview;
use crate::outbound::SharedOutboundRouter;
use crate::runtime::{
    IdempotencyKey, NewTrigger, PromptError, PromptRequestId, ResponseChunk, SharedDagBudget,
    SharedPromptQueue, SharedResponseSink,
};
use crate::threads::{
    AgentThreadId, MessageKind, NewMessage, SharedThreadStore, ThreadId, ThreadMessageId,
};
use crate::types::{PROMPT_MAX_BYTES, Participant, Prompt, ToolName};

use super::super::traits::{Tool, ToolCallContext, ToolError};

#[derive(Debug, Deserialize)]
struct SendMessageInput {
    /// Who to address — a colleague id (human or agent) from the `<colleagues>`
    /// roster. Omit entirely to post an untagged message to the thread.
    #[serde(default)]
    receiver: Option<ColleagueId>,
    /// The message body. Same `PROMPT_MAX_BYTES` cap as the HTTP boundary.
    content: String,
    /// Where to deliver (#178). Omitted = the current thread (back-compat). A
    /// `channel` target starts a NEW thread in that channel (membership-gated); a
    /// `dm` target opens a 1:1 with a human.
    #[serde(default)]
    to: Option<Target>,
}

/// A `send_message` destination (#178). Externally tagged so the wire shape is
/// `{ "channel": "<uuid>" }` or `{ "dm": "<uuid>" }` — a sum, not two optional
/// fields (CLAUDE.md §1).
#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Target {
    /// Start a new top-level thread in a channel the agent is a member of.
    Channel(ChannelId),
    /// Open / address a 1:1 DM with a human colleague.
    Dm(ColleagueId),
}

#[derive(Debug, Serialize)]
struct SendMessageOutput {
    thread_id: ThreadId,
    request_id: Option<PromptRequestId>,
    delivery: &'static str,
}

/// Agent communication tool.
///
/// Holds shared handles to the collaborators it needs: threads (post + resolve
/// participation + membership gate), queue (enqueue agent trigger), dag (budget
/// bump), colleagues (resolve receiver colleague by id), sink (terminal failure
/// publish on budget exhaustion).
pub struct SendMessageTool {
    name: ToolName,
    description: &'static str,
    input_schema: Arc<Value>,
    threads: SharedThreadStore,
    queue: SharedPromptQueue,
    dag: SharedDagBudget,
    colleagues: SharedColleagueStore,
    sink: SharedResponseSink,
    /// Ensures the resolved thread reaches its external surface (#178).
    outbound: SharedOutboundRouter,
}

const TOOL_NAME: &str = "send_message";

/// Surfaced both at validation time and from the dispatch match. System is the
/// synthetic counterpart for background cognition and never receives deliveries.
const ERR_SYSTEM_RECEIVER: &str = "send_message: cannot deliver to System";

const TOOL_DESCRIPTION: &str = "Send a message. Use this for ALL communication — \
    plain assistant text is not delivered. Choose the most appropriate place, \
    like a colleague would. Arguments: `receiver` (optional) is the colleague id \
    (a uuid) shown in your `<colleagues>` block — any colleague, human or agent; \
    to reply to the person who prompted you, use their id from `<speaking-with>`. \
    Omit `receiver` to post untagged. `content` is the message body. `to` \
    (optional) chooses where to deliver: omit it to post in the CURRENT thread; \
    `{\"channel\": \"<id>\"}` starts a NEW thread in a channel from your \
    `<channels>` block (you must be a member); `{\"dm\": \"<id>\"}` opens a direct \
    message with a human. You cannot message a human who is not a member of the \
    channel, post to a channel you do not belong to, or DM an agent.";

impl std::fmt::Debug for SendMessageTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SendMessageTool").finish_non_exhaustive()
    }
}

impl SendMessageTool {
    /// Construct the tool from its shared collaborators.
    #[must_use]
    pub fn new(
        threads: SharedThreadStore,
        queue: SharedPromptQueue,
        dag: SharedDagBudget,
        colleagues: SharedColleagueStore,
        sink: SharedResponseSink,
        outbound: SharedOutboundRouter,
    ) -> Self {
        let name = ToolName::try_from(TOOL_NAME).expect("invariant: send_message is a valid name");
        let input_schema = Arc::new(json!({
            "type": "object",
            "required": ["content"],
            "properties": {
                "receiver": {
                    "type": "string",
                    "format": "uuid",
                    "description": "Colleague id (human or agent) from your <colleagues> block. Omit to post untagged to the thread."
                },
                "content": { "type": "string", "maxLength": PROMPT_MAX_BYTES },
                "to": {
                    "description": "Where to deliver. Omit for the current thread. {\"channel\": \"<uuid>\"} starts a new thread in a channel you belong to; {\"dm\": \"<uuid>\"} DMs a human.",
                    "type": "object",
                    "properties": {
                        "channel": { "type": "string", "format": "uuid" },
                        "dm": { "type": "string", "format": "uuid" }
                    },
                    "additionalProperties": false
                }
            },
            "additionalProperties": false
        }));
        Self {
            name,
            description: TOOL_DESCRIPTION,
            input_schema,
            threads,
            queue,
            dag,
            colleagues,
            sink,
            outbound,
        }
    }

    /// Up-front input validation. Returns `(content, receiver, viewer_agent_id)`
    /// — `None` receiver is a valid untagged post; `viewer_agent_id` is the
    /// authoring agent (the caller must be an agent), reused as the egress
    /// `from`. Resolves the wire-side receiver to a typed [`Participant`] and
    /// rejects self / System targets.
    async fn validate(
        &self,
        content: String,
        receiver: Option<ColleagueId>,
        ctx: &ToolCallContext,
    ) -> Result<(Prompt, Option<Participant>, AgentId), ToolError> {
        let content = Prompt::try_from(content).map_err(|e| {
            set_outcome("invalid_input");
            ToolError::InvalidInput(e.to_string())
        })?;

        // Caller must be an agent — humans don't run tool calls.
        let viewer_agent_id = ctx.viewer.agent_id().ok_or_else(|| {
            set_outcome("invalid_input");
            ToolError::InvalidInput("send_message: caller must be an agent".into())
        })?;

        let Some(id) = receiver else {
            return Ok((content, None, viewer_agent_id));
        };
        let receiver = self.resolve_colleague(id, ctx).await?;

        if receiver == ctx.viewer {
            set_outcome("invalid_input");
            return Err(ToolError::InvalidInput(
                "send_message: receiver equals caller".into(),
            ));
        }
        if receiver.is_system() {
            set_outcome("invalid_input");
            return Err(ToolError::InvalidInput(ERR_SYSTEM_RECEIVER.into()));
        }
        Ok((content, Some(receiver), viewer_agent_id))
    }

    /// Resolve a `receiver` colleague id into a [`Participant`] (its kind —
    /// human or agent — comes from the row). Privileged directory read; a
    /// colleague outside the caller's org is reported as unknown (no existence
    /// leak).
    async fn resolve_colleague(
        &self,
        id: ColleagueId,
        ctx: &ToolCallContext,
    ) -> Result<Participant, ToolError> {
        let colleague = self.colleagues.read(id).await.map_err(|e| match e {
            ColleagueError::NotFound(_) => {
                set_outcome("unknown_colleague");
                warn!(patom.colleague.id = %id, "send_message.unknown_colleague");
                ToolError::InvalidInput(format!("send_message: unknown colleague {id}"))
            }
            err => {
                set_outcome("backend_error");
                warn!(error = %err, patom.colleague.id = %id, "send_message.colleague_lookup_failed");
                ToolError::Backend(format!("send_message: colleague lookup: {err}"))
            }
        })?;
        if colleague.org_id() != ctx.org_id {
            set_outcome("unknown_colleague");
            warn!(patom.colleague.id = %id, "send_message.colleague_cross_org");
            return Err(ToolError::InvalidInput(format!(
                "send_message: unknown colleague {id}"
            )));
        }
        Ok(Participant::from(&colleague))
    }

    #[tracing::instrument(
        skip_all,
        name = "tool.send_message",
        fields(
            patom.thread.id = tracing::field::Empty,
            patom.from.viewer = %ctx.viewer,
            patom.send_message.outcome = tracing::field::Empty,
        ),
    )]
    async fn handle(
        &self,
        input: SendMessageInput,
        ctx: &ToolCallContext,
    ) -> Result<SendMessageOutput, ToolError> {
        let SendMessageInput {
            receiver,
            content,
            to,
        } = input;
        // `from` is the authoring agent, validated + returned by `validate`.
        let (content, receiver, from) = self.validate(content, receiver, ctx).await?;
        let caller = Caller::new(ctx.acting_user_id, ctx.org_id);
        // Resolve where to deliver: the current thread (default) or a freshly
        // located/created channel / DM thread (#178).
        let thread = self
            .resolve_or_create_target(ctx, &caller, from, to)
            .await?;
        tracing::Span::current().record("patom.thread.id", tracing::field::display(thread));
        let sender = ctx.viewer.colleague_id();

        let out = match receiver {
            None => {
                self.post(
                    &caller,
                    thread,
                    from,
                    sender,
                    None,
                    &content,
                    ctx.request_id,
                )
                .await?;
                set_outcome("posted_untagged");
                self.posted(thread, None)
            }
            Some(Participant::Human {
                colleague_id,
                user_id,
            }) => {
                self.deliver_to_human(
                    &caller,
                    ctx,
                    thread,
                    from,
                    sender,
                    colleague_id,
                    user_id,
                    &content,
                )
                .await?
            }
            Some(Participant::Agent {
                colleague_id,
                agent_id,
            }) => {
                self.deliver_to_agent(
                    &caller,
                    ctx,
                    thread,
                    from,
                    sender,
                    colleague_id,
                    agent_id,
                    &content,
                )
                .await?
            }
            Some(Participant::System) => {
                set_outcome("invalid_input");
                return Err(ToolError::InvalidInput(ERR_SYSTEM_RECEIVER.into()));
            }
        };

        // Ensure the resolved thread reaches its external surface (Lark/Discord).
        // Best-effort: the durable feed row + web SSE already happened; a stuck
        // surface must not fail the tool call. The composite logs per-surface
        // failures and is a no-op for a web-only thread (#178).
        let _ = self.outbound.ensure_delivery(ctx.org_id, thread).await;
        Ok(out)
    }

    /// Resolve the destination thread: the current thread (`to` omitted), a new
    /// top-level thread in a channel the agent belongs to, or a 1:1 DM with a
    /// human. Membership-gates a channel target; rejects an agent DM target
    /// (agent↔agent stays in shared channels/threads). For a created thread the
    /// agent's participation is resolved so the worker can drive it.
    async fn resolve_or_create_target(
        &self,
        ctx: &ToolCallContext,
        caller: &Caller,
        from: AgentId,
        to: Option<Target>,
    ) -> Result<ThreadId, ToolError> {
        let Some(target) = to else {
            return ctx.thread_id.ok_or_else(|| {
                set_outcome("invalid_input");
                ToolError::Backend("send_message: no thread context on this call".into())
            });
        };
        let creator = ctx.viewer.colleague_id().ok_or_else(|| {
            set_outcome("invalid_input");
            ToolError::InvalidInput("send_message: caller must be a colleague".into())
        })?;
        let (channel_id, dm_counterpart) = match target {
            Target::Channel(channel_id) => {
                let member = self
                    .threads
                    .colleague_in_channel(channel_id, creator)
                    .await
                    .map_err(|e| {
                        set_outcome("backend_error");
                        ToolError::Backend(format!("send_message: membership check: {e}"))
                    })?;
                if !member {
                    set_outcome("agent_not_member");
                    return Err(ToolError::InvalidInput(
                        "send_message: you are not a member of that channel".into(),
                    ));
                }
                (Some(channel_id), None)
            }
            Target::Dm(counterpart) => {
                let participant = self.resolve_colleague(counterpart, ctx).await?;
                let Participant::Human { colleague_id, .. } = participant else {
                    set_outcome("invalid_input");
                    return Err(ToolError::InvalidInput(
                        "send_message: a dm target must be a human colleague".into(),
                    ));
                };
                (None, Some(colleague_id))
            }
        };
        let thread = self
            .threads
            .create_thread(caller, channel_id, None, creator, dm_counterpart)
            .await
            .map_err(|e| {
                set_outcome("backend_error");
                ToolError::Backend(format!("send_message: create thread: {e}"))
            })?;
        self.threads
            .resolve_participation(caller, thread, from)
            .await
            .map_err(|e| {
                set_outcome("backend_error");
                ToolError::Backend(format!("send_message: resolve participation: {e}"))
            })?;
        Ok(thread)
    }

    /// Human receiver: gate on channel membership (no auto-add), then post.
    #[allow(clippy::too_many_arguments)]
    async fn deliver_to_human(
        &self,
        caller: &Caller,
        ctx: &ToolCallContext,
        thread: ThreadId,
        from: AgentId,
        sender: Option<ColleagueId>,
        colleague_id: ColleagueId,
        user_id: crate::auth::UserId,
        content: &Prompt,
    ) -> Result<SendMessageOutput, ToolError> {
        let member = self
            .threads
            .is_channel_member(thread, user_id)
            .await
            .map_err(|e| {
                set_outcome("backend_error");
                ToolError::Backend(format!("send_message: membership check failed: {e}"))
            })?;
        if !member {
            set_outcome("human_not_member");
            warn!(patom.colleague.id = %colleague_id, "send_message.human_not_member");
            return Err(ToolError::InvalidInput(
                "send_message: recipient is not a member of this channel".into(),
            ));
        }
        self.post(
            caller,
            thread,
            from,
            sender,
            Some(colleague_id),
            content,
            ctx.request_id,
        )
        .await?;
        set_outcome("human_delivered");
        info!(text.preview = %preview(content.as_str()), "send_message.delivered_to_human");
        Ok(self.posted(thread, None))
    }

    /// Agent receiver: post the egress row and resolve the receiver's
    /// participation (independent writes, run concurrently), then bump the DAG
    /// budget and enqueue the wake-up trigger.
    #[allow(clippy::too_many_arguments)]
    async fn deliver_to_agent(
        &self,
        caller: &Caller,
        ctx: &ToolCallContext,
        thread: ThreadId,
        from: AgentId,
        sender: Option<ColleagueId>,
        colleague_id: ColleagueId,
        agent_id: AgentId,
        content: &Prompt,
    ) -> Result<SendMessageOutput, ToolError> {
        let (posted, state) = tokio::join!(
            self.post(
                caller,
                thread,
                from,
                sender,
                Some(colleague_id),
                content,
                ctx.request_id
            ),
            self.threads.resolve_participation(caller, thread, agent_id),
        );
        let posted = posted?;
        let state = state.map_err(|e| {
            set_outcome("participation_failed");
            ToolError::Backend(format!("send_message: resolve participation: {e}"))
        })?;
        self.bump_dag_budget(ctx).await?;
        let request_id = self
            .enqueue_agent_trigger(ctx, thread, state, agent_id, sender, posted)
            .await?;
        set_outcome("agent_delivered");
        info!(
            patom.request.id = %request_id,
            patom.to.agent.id = %agent_id,
            text.preview = %preview(content.as_str()),
            "send_message.delivered",
        );
        Ok(self.posted(thread, Some(request_id)))
    }

    /// Append the posted egress row to the thread feed, then publish an
    /// [`ResponseChunk::AgentMessage`] on the current request so live consumers
    /// (the Slack stream pump, the web SSE) see it without refetching G2. The
    /// publish is best-effort — the durable delivery is the feed row; a closed
    /// or absent stream is benign. Returns the row's surface id.
    #[allow(clippy::too_many_arguments)]
    async fn post(
        &self,
        caller: &Caller,
        thread: ThreadId,
        from: AgentId,
        sender: Option<ColleagueId>,
        receiver: Option<ColleagueId>,
        content: &Prompt,
        request_id: PromptRequestId,
    ) -> Result<ThreadMessageId, ToolError> {
        let id = self
            .threads
            .append(
                caller,
                thread,
                NewMessage {
                    kind: MessageKind::Posted,
                    sender,
                    owner_agent_id: None,
                    receiver,
                    body: outbound_chat_message(content.as_str()),
                    request_id: Some(request_id),
                    idempotency_key: None,
                },
            )
            .await
            .map_err(|e| {
                set_outcome("post_failed");
                ToolError::Backend(format!("send_message: post failed: {e}"))
            })?;
        let _ = self
            .sink
            .publish_for_user(
                caller.user_id,
                request_id,
                ResponseChunk::AgentMessage {
                    from,
                    to_thread: thread,
                    content: content.as_str().to_owned(),
                    to: receiver,
                },
            )
            .await;
        Ok(id)
    }

    /// Build the success output for a posted message.
    fn posted(&self, thread: ThreadId, request_id: Option<PromptRequestId>) -> SendMessageOutput {
        SendMessageOutput {
            thread_id: thread,
            request_id,
            delivery: if request_id.is_some() {
                "queued"
            } else {
                "posted"
            },
        }
    }

    /// Enqueue the wake-up trigger for an agent receiver. The trigger inherits
    /// the caller's DAG root and points at the posted row as its
    /// `trigger_message_id`; idempotency is `tag:{thread}:{agent}:{message}`.
    async fn enqueue_agent_trigger(
        &self,
        ctx: &ToolCallContext,
        thread: ThreadId,
        state: AgentThreadId,
        agent_id: AgentId,
        sender: Option<ColleagueId>,
        trigger_msg: ThreadMessageId,
    ) -> Result<PromptRequestId, ToolError> {
        let sender_colleague = sender.ok_or_else(|| {
            ToolError::Backend("send_message: agent caller missing colleague".into())
        })?;
        let key = format!(
            "tag:{}:{}:{}",
            thread.as_uuid(),
            agent_id.as_uuid(),
            trigger_msg.as_uuid(),
        );
        let key = IdempotencyKey::try_from(key)
            .map_err(|e| ToolError::Backend(format!("send_message: bad idempotency: {e}")))?;
        self.queue
            .enqueue_trigger(NewTrigger {
                org_id: ctx.org_id,
                acting_user_id: ctx.acting_user_id,
                thread_id: Some(thread),
                state_id: Some(state),
                background_turn_id: None,
                sender_colleague_id: sender_colleague,
                receiver_agent_id: agent_id,
                root_request_id: Some(ctx.root_request_id),
                trigger_message_id: Some(trigger_msg),
                idempotency_key: key,
                kind_payload: crate::runtime::RequestKindPayload::Normal {},
            })
            .await
            .map_err(|e| {
                set_outcome("enqueue_failed");
                ToolError::Backend(format!("send_message: enqueue failed: {e}"))
            })
    }

    /// Atomically bump the DAG turn budget for an agent-spawning delivery. On
    /// exceed we surface a terminal failure on the root stream and reject; the
    /// posted row stays (the cap rejects future turns, not this message).
    async fn bump_dag_budget(&self, ctx: &ToolCallContext) -> Result<(), ToolError> {
        match self
            .dag
            .bump_or_fail_for_user(ctx.acting_user_id, ctx.root_request_id)
            .await
        {
            Ok(_) => Ok(()),
            Err(e @ PromptError::DagBudgetExceeded { .. }) => {
                set_outcome("dag_exceeded");
                warn!(error = %e, patom.dag.root = %ctx.root_request_id, "send_message.dag.exceeded");
                let chunk =
                    ResponseChunk::from_failure(&crate::runtime::FailureReason::DagBudgetExceeded);
                let _ = self
                    .sink
                    .publish_for_user(ctx.acting_user_id, ctx.root_request_id, chunk)
                    .await;
                let _ = self
                    .sink
                    .close_for_user(ctx.acting_user_id, ctx.root_request_id)
                    .await;
                Err(ToolError::InvalidInput(format!(
                    "send_message: dag budget exceeded: {e}"
                )))
            }
            Err(e) => {
                set_outcome("dag_failed");
                warn!(error = %e, patom.dag.root = %ctx.root_request_id, "send_message.dag.failed");
                Err(ToolError::Backend(format!(
                    "send_message: dag bump failed: {e}"
                )))
            }
        }
    }
}

/// Record the `patom.send_message.outcome` field on the enclosing span. Each
/// decision point labels its branch so dashboards can `GROUP BY` it. Variants:
/// `posted_untagged`, `human_delivered`, `human_not_member`, `agent_delivered`,
/// `dag_exceeded`, `dag_failed`, `unknown_agent`, `unknown_colleague`,
/// `invalid_input`, `backend_error`, `participation_failed`, `post_failed`,
/// `enqueue_failed`.
fn set_outcome(label: &'static str) {
    tracing::Span::current().record("patom.send_message.outcome", label);
}

#[async_trait]
impl Tool for SendMessageTool {
    fn name(&self) -> &ToolName {
        &self.name
    }

    fn description(&self) -> &str {
        self.description
    }

    fn input_schema(&self) -> Arc<Value> {
        self.input_schema.clone()
    }

    async fn execute(&self, input: Value, ctx: &ToolCallContext) -> Result<String, ToolError> {
        let parsed: SendMessageInput = serde_json::from_value(input)?;
        let out = self.handle(parsed, ctx).await?;
        let body = serde_json::to_string(&out)?;
        Ok(body)
    }
}

/// Render the outbound message as a single text-block user-content row. Stored
/// under the caller's sender colleague; the feed's viewer mapping renders it as
/// Assistant for the author and User for everyone else.
fn outbound_chat_message(content: &str) -> crate::provider::ChatMessage {
    use crate::provider::{ChatMessage, UserContent};
    ChatMessage::User(vec![UserContent::Text(content.to_string())])
}
