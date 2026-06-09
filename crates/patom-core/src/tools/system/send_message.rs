//! `send_message` — the agent's SOLE output verb in the thread-feed model.
//!
//! See SPEC §3 + doc/thread-chat-refactor.md §2. Plain assistant text is a
//! private turn artifact; communication happens only through this tool, which
//! appends a `kind='posted'` row to the thread feed (the egress the worker's
//! ping-pong guard watches for).
//!
//! Three receiver shapes, all posting to the *current* thread (`ctx.thread_id`):
//!
//! - **none** (`receiver` omitted) — an untagged post (announcement / thinking
//!   aloud to the channel). Posts and returns.
//! - **human** — agents reach a human only inside a thread the human can see:
//!   a channel thread requires channel membership (`ThreadStore::is_channel_member`),
//!   rejected with no auto-add; a DM thread is always reachable. Posts a
//!   `receiver`-addressed row.
//! - **agent** — agents are org-global (reachable in any thread, no membership).
//!   Posts the row, resolves the receiver's `(thread, agent)` participation,
//!   bumps the DAG turn budget, and enqueues a `prompt_requests` *trigger* whose
//!   `trigger_message_id` is the posted row; the worker picks it up.
//!
//! Returns the thread id and (for agent receivers) the trigger request id.

use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tracing::{info, warn};

use crate::agents::{AgentId, AgentName, AgentStoreError, SharedAgentStore};
use crate::auth::Caller;
use crate::colleagues::{ColleagueError, ColleagueId, SharedColleagueStore};
use crate::observability::log::preview;
use crate::runtime::{
    IdempotencyKey, NewTrigger, PromptError, PromptRequestId, ResponseChunk, SharedDagBudget,
    SharedPromptQueue, SharedResponseSink,
};
use crate::threads::{
    AgentThreadId, MessageKind, NewMessage, SharedThreadStore, ThreadId, ThreadMessageId,
};
use crate::types::{PROMPT_MAX_BYTES, Participant, Prompt, ToolName};

use super::super::traits::{Tool, ToolCallContext, ToolError};

/// Wire-side receiver shape. Three forms, in preference order:
///
/// - `{"kind":"colleague","id":"<uuid>"}` — **canonical**. Any colleague (human
///   or agent) by the id surfaced in the `<colleagues>` roster.
/// - `{"kind":"agent","name":"<role>"}` — sugar. An agent by role name
///   (case-insensitive, scoped to the caller's org).
/// - `{"kind":"human"}` — sugar for the DAG-root human (the user under whose
///   authority the agent runs).
#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum SendMessageReceiver {
    Colleague { id: ColleagueId },
    Agent { name: AgentName },
    Human,
}

#[derive(Debug, Deserialize)]
struct SendMessageInput {
    /// Who to address. Omit entirely to post an untagged message to the thread.
    #[serde(default)]
    receiver: Option<SendMessageReceiver>,
    /// The message body. Same `PROMPT_MAX_BYTES` cap as the HTTP boundary.
    content: String,
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
/// bump), agent store (resolve receiver agent), colleagues (resolve receiver
/// colleague), sink (terminal failure publish on budget exhaustion).
pub struct SendMessageTool {
    name: ToolName,
    description: &'static str,
    input_schema: Arc<Value>,
    threads: SharedThreadStore,
    queue: SharedPromptQueue,
    dag: SharedDagBudget,
    agents: SharedAgentStore,
    colleagues: SharedColleagueStore,
    sink: SharedResponseSink,
}

const TOOL_NAME: &str = "send_message";

/// Surfaced both at validation time and from the dispatch match. System is the
/// synthetic counterpart for background cognition and never receives deliveries.
const ERR_SYSTEM_RECEIVER: &str = "send_message: cannot deliver to System";

const TOOL_DESCRIPTION: &str = "Send a message in the current thread. \
    Use this for ALL communication — plain assistant text is not delivered. \
    Arguments: `receiver` (optional) is `{\"kind\":\"colleague\",\"id\":\"<uuid>\"}` to \
    address any colleague (human or agent) by the id shown in your `<colleagues>` \
    block, `{\"kind\":\"agent\",\"name\":\"<role>\"}` to reach an agent by role \
    name, or `{\"kind\":\"human\"}` to reply to the person who prompted you; omit \
    `receiver` to post an untagged message to the thread. `content` is the message \
    body. You cannot message a human who is not a member of this channel.";

impl std::fmt::Debug for SendMessageTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SendMessageTool").finish_non_exhaustive()
    }
}

impl SendMessageTool {
    /// Construct the tool from its six shared collaborators.
    #[must_use]
    pub fn new(
        threads: SharedThreadStore,
        queue: SharedPromptQueue,
        dag: SharedDagBudget,
        agents: SharedAgentStore,
        colleagues: SharedColleagueStore,
        sink: SharedResponseSink,
    ) -> Self {
        let name = ToolName::try_from(TOOL_NAME).expect("invariant: send_message is a valid name");
        let input_schema = Arc::new(json!({
            "type": "object",
            "required": ["content"],
            "properties": {
                "receiver": {
                    "type": "object",
                    "oneOf": [
                        {
                            "required": ["kind", "id"],
                            "properties": {
                                "kind": { "const": "colleague" },
                                "id": { "type": "string", "format": "uuid" }
                            },
                            "additionalProperties": false
                        },
                        {
                            "required": ["kind", "name"],
                            "properties": {
                                "kind": { "const": "agent" },
                                "name": { "type": "string", "minLength": 1 }
                            },
                            "additionalProperties": false
                        },
                        {
                            "required": ["kind"],
                            "properties": { "kind": { "const": "human" } },
                            "additionalProperties": false
                        }
                    ]
                },
                "content": { "type": "string", "maxLength": PROMPT_MAX_BYTES }
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
            agents,
            colleagues,
            sink,
        }
    }

    /// Up-front input validation. Returns `(content, receiver, viewer_agent_id)`
    /// — `None` receiver is a valid untagged post; `viewer_agent_id` is the
    /// authoring agent (the caller must be an agent), reused as the egress
    /// `from`. Resolves the wire-side receiver to a typed [`Participant`] and
    /// rejects self / System targets.
    async fn validate(
        &self,
        input: SendMessageInput,
        ctx: &ToolCallContext,
    ) -> Result<(Prompt, Option<Participant>, AgentId), ToolError> {
        let content = Prompt::try_from(input.content).map_err(|e| {
            set_outcome("invalid_input");
            ToolError::InvalidInput(e.to_string())
        })?;

        // Caller must be an agent — humans don't run tool calls.
        let viewer_agent_id = ctx.viewer.agent_id().ok_or_else(|| {
            set_outcome("invalid_input");
            ToolError::InvalidInput("send_message: caller must be an agent".into())
        })?;

        let Some(raw) = input.receiver else {
            return Ok((content, None, viewer_agent_id));
        };
        let receiver = self.resolve_receiver(viewer_agent_id, raw, ctx).await?;

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

    /// Resolve the wire-side receiver into a [`Participant`].
    async fn resolve_receiver(
        &self,
        viewer: AgentId,
        raw: SendMessageReceiver,
        ctx: &ToolCallContext,
    ) -> Result<Participant, ToolError> {
        let name = match raw {
            SendMessageReceiver::Colleague { id } => {
                return self.resolve_colleague(id, ctx).await;
            }
            SendMessageReceiver::Human => {
                let cid = self
                    .colleagues
                    .resolve_user(ctx.org_id, ctx.acting_user_id)
                    .await
                    .map_err(|e| {
                        set_outcome("backend_error");
                        ToolError::Backend(format!("send_message: resolve human colleague: {e}"))
                    })?;
                return Ok(Participant::human(cid, ctx.acting_user_id));
            }
            SendMessageReceiver::Agent { name } => name,
        };
        let record = self
            .agents
            .read_by_name_for_viewer(viewer, &name)
            .await
            .map_err(|e| match e {
                AgentStoreError::NameNotFound(_) => {
                    set_outcome("unknown_agent");
                    warn!(patom.agent.name = %name, "send_message.unknown_agent");
                    ToolError::InvalidInput(format!("send_message: unknown agent name {name}"))
                }
                err => {
                    set_outcome("backend_error");
                    warn!(error = %err, patom.agent.name = %name, "send_message.agent_lookup_failed");
                    ToolError::Backend(format!("send_message: agent lookup: {err}"))
                }
            })?;
        let cid = self
            .colleagues
            .resolve_agent(record.org_id, record.id)
            .await
            .map_err(|e| {
                set_outcome("backend_error");
                ToolError::Backend(format!("send_message: resolve agent colleague: {e}"))
            })?;
        Ok(Participant::agent(cid, record.id))
    }

    /// Resolve a canonical `{"kind":"colleague","id":…}` receiver. Privileged
    /// directory read; a colleague outside the caller's org is reported as
    /// unknown (no existence leak).
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
        // `from` is the authoring agent, validated + returned by `validate`.
        let (content, receiver, from) = self.validate(input, ctx).await?;
        // The egress posts to the thread the claim is running in.
        let thread = ctx.thread_id.ok_or_else(|| {
            set_outcome("invalid_input");
            ToolError::Backend("send_message: no thread context on this call".into())
        })?;
        tracing::Span::current().record("patom.thread.id", tracing::field::display(thread));
        let caller = Caller::new(ctx.acting_user_id, ctx.org_id);
        let sender = ctx.viewer.colleague_id();

        match receiver {
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
                Ok(self.posted(thread, None))
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
                .await
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
                .await
            }
            Some(Participant::System) => {
                set_outcome("invalid_input");
                Err(ToolError::InvalidInput(ERR_SYSTEM_RECEIVER.into()))
            }
        }
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
