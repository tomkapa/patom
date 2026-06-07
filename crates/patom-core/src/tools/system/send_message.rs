//! `send_message` — agent → agent / agent → human delivery.
//!
//! See SPEC §3 of the multi-agent design. This is the *only* mechanism by
//! which an agent communicates: plain assistant text is private to the turn
//! and never delivered.
//!
//! Execution path (in one tool call, all-or-nothing):
//!
//! 1. Validate input (size caps, receiver shape; refuse self-messages).
//! 2. Resolve-or-create the receiver session for the caller's DAG via
//!    [`SessionStore::resolve_or_create_for_pair`]. The store's upsert
//!    canonicalises the pair so two callers naming the same conversation
//!    converge on the same row.
//! 3. If the session was freshly minted *and* `context_summary` is set,
//!    append a `system`-kind opening row recording the framing — this is
//!    what the receiver sees as user-side context on its first turn.
//! 4. For Human receivers in a *different* session (e.g. a descendant agent
//!    first reaching the human), append the outbound message (sender =
//!    caller, receiver = the addressee). Skip this append for Agent
//!    receivers (the worker's `agent.reply` re-appends the prompt when it
//!    claims the queued row) and for Human receivers in the *same* session
//!    (the caller's `Assistant([…, ToolCall])` row already persisted by
//!    `turn.rs` carries the message text). In both skip cases a double
//!    append would split the assistant `tool_calls` from the matching
//!    `tool_result` on the next turn's wire payload.
//! 5. Atomically bump the DAG turn budget. On `DagBudgetExceeded` the tool
//!    returns an error so the model sees the rejection; we do *not* roll
//!    the appended rows back — the bump cap rejects future calls rather
//!    than reverse this one.
//! 6. For Agent receivers, enqueue a `prompt_requests` row; the worker
//!    picks it up. For Human receivers, publish a non-terminal
//!    [`ResponseChunk::AgentMessage`] on the root request's stream so the
//!    SSE client sees the reply on the same connection it opened on POST.
//!
//! Returns the receiver's session id and (for Agent receivers) the new
//! request id so the model can cross-reference them in subsequent turns.

use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tracing::{debug, info, warn};

use crate::agents::{AgentId, AgentName, AgentStoreError, SharedAgentStore};
use crate::colleagues::{ColleagueError, ColleagueId, ColleagueKind};
use crate::observability::log::preview;
use crate::runtime::IdempotencyKey;
use crate::runtime::PromptError;
use crate::runtime::{
    CONTEXT_SUMMARY_MAX_BYTES, NewPromptRequest, PromptRequestId, ResponseChunk, SharedDagBudget,
    SharedPromptQueue, SharedResponseSink,
};
use crate::session::{SessionId, SharedSessionStore};
use crate::types::{MessageSender, PROMPT_MAX_BYTES, Participant, Prompt, ToolName};

use super::super::traits::{Tool, ToolCallContext, ToolError};

/// Wire-side receiver shape. Three forms, in preference order:
///
/// - `{"kind":"colleague","id":"<uuid>"}` — **canonical**. Addresses any
///   colleague (human or agent) by the id surfaced in the `<agents>` roster.
///   This is how the agent reaches a *specific* human coworker, not just the
///   anonymous root human.
/// - `{"kind":"agent","name":"<role>"}` — sugar. Resolves an agent by role name
///   (case-insensitive, scoped to the caller's org) for the common "reply to a
///   named peer" path without looking the id up.
/// - `{"kind":"human"}` — sugar for the DAG-root human (the user under whose
///   authority the agent runs), so "reply to whoever prompted me" needs no id.
#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum SendMessageReceiver {
    Colleague { id: ColleagueId },
    Agent { name: AgentName },
    Human,
}

#[derive(Debug, Deserialize)]
struct SendMessageInput {
    /// Who to send to — `{"kind":"human"}` or
    /// `{"kind":"agent","name":"<role>"}`.
    receiver: SendMessageReceiver,
    /// The message body. Same `PROMPT_MAX_BYTES` cap as the HTTP boundary.
    content: String,
    /// REQUIRED only the first time you message this receiver in the current
    /// task — a brief framing of why you're contacting them and what you
    /// need. The system stores it as the opening note on the new session.
    /// IGNORED on follow-ups; the system drops the field.
    #[serde(default)]
    context_summary: Option<String>,
}

#[derive(Debug, Serialize)]
struct SendMessageOutput {
    session_id: SessionId,
    request_id: Option<PromptRequestId>,
    delivery: &'static str,
}

/// Agent communication tool.
///
/// Holds shared handles to the four collaborators it needs: sessions
/// (resolve-or-create + append), queue (enqueue receiver row), dag (budget
/// bump), agent store (validate receiver agent_id).
pub struct SendMessageTool {
    name: ToolName,
    description: &'static str,
    input_schema: Arc<Value>,
    sessions: SharedSessionStore,
    queue: SharedPromptQueue,
    dag: SharedDagBudget,
    agents: SharedAgentStore,
    /// Resolves Human/Agent receivers to colleague-backed `Participant`s for
    /// the new schema (Stage 3).
    colleagues: crate::colleagues::SharedColleagueStore,
    /// Publish-side handle on the response broadcast hub. Human-receiver
    /// deliveries publish a [`ResponseChunk::AgentMessage`] on the root
    /// request's stream so the SSE client sees the agent's message as a
    /// non-terminal chunk on the same connection it opened on POST.
    sink: SharedResponseSink,
}

const TOOL_NAME: &str = "send_message";

/// Surfaced both at validation time (early reject) and from the dispatch
/// match (defence in depth). System is the synthetic counterpart for
/// reflection / resolution sessions and never receives deliveries.
const ERR_SYSTEM_RECEIVER: &str = "send_message: cannot deliver to System";

const TOOL_DESCRIPTION: &str = "Send a message to a participant. \
    Use this for ALL communication including replies to the human — plain \
    assistant text is not delivered. \
    Arguments: `receiver` is `{\"kind\":\"colleague\",\"id\":\"<uuid>\"}` to \
    address any colleague (human or agent) by the id shown in your `<agents>` \
    block, `{\"kind\":\"agent\",\"name\":\"<role>\"}` to reach an agent by role \
    name, or `{\"kind\":\"human\"}` to reply to the person who prompted you; \
    `content` is the message body; `context_summary` is REQUIRED only the \
    first time you message this receiver in the current task — a brief \
    framing of why you're contacting them and what you need (IGNORE on \
    follow-ups, the system drops it). \
    The system decides whether a session already exists; do not specify a \
    session id.";

impl std::fmt::Debug for SendMessageTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SendMessageTool").finish_non_exhaustive()
    }
}

impl SendMessageTool {
    /// Construct the tool from its five shared collaborators.
    #[must_use]
    pub fn new(
        sessions: SharedSessionStore,
        queue: SharedPromptQueue,
        dag: SharedDagBudget,
        agents: SharedAgentStore,
        colleagues: crate::colleagues::SharedColleagueStore,
        sink: SharedResponseSink,
    ) -> Self {
        let name = ToolName::try_from(TOOL_NAME).expect("invariant: send_message is a valid name");
        let input_schema = Arc::new(json!({
            "type": "object",
            "required": ["receiver", "content"],
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
                "content": { "type": "string", "maxLength": PROMPT_MAX_BYTES },
                "context_summary": {
                    "type": ["string", "null"],
                    "maxLength": CONTEXT_SUMMARY_MAX_BYTES
                }
            },
            "additionalProperties": false
        }));
        Self {
            name,
            description: TOOL_DESCRIPTION,
            input_schema,
            sessions,
            queue,
            dag,
            agents,
            colleagues,
            sink,
        }
    }

    /// Up-front input validation. Returns `(content, context_summary,
    /// receiver)` so the main path is straight-line. The wire-side
    /// `{kind:"agent", name:<role>}` shape resolves through
    /// [`Self::resolve_receiver`] before any session / queue work.
    async fn validate(
        &self,
        input: SendMessageInput,
        ctx: &ToolCallContext,
    ) -> Result<(Prompt, Option<String>, Participant), ToolError> {
        // §1: parse, don't validate. Bound everything at the boundary.
        let content = Prompt::try_from(input.content).map_err(|e| {
            set_outcome("invalid_input");
            ToolError::InvalidInput(e.to_string())
        })?;
        if let Some(s) = input.context_summary.as_deref()
            && s.len() > CONTEXT_SUMMARY_MAX_BYTES
        {
            set_outcome("invalid_input");
            return Err(ToolError::InvalidInput(format!(
                "context_summary exceeds cap ({CONTEXT_SUMMARY_MAX_BYTES} bytes)"
            )));
        }

        // Caller must be an agent — humans don't run tool calls. This also
        // means we always have a `caller_agent_id` to record on the session
        // and (for human receiver) to attribute the AgentMessage chunk.
        if !ctx.viewer.is_agent() {
            set_outcome("invalid_input");
            return Err(ToolError::InvalidInput(
                "send_message: caller must be an agent".into(),
            ));
        }

        let viewer_agent_id = ctx
            .viewer
            .agent_id()
            .expect("invariant: viewer guard above rejects non-agent callers");
        let receiver = self
            .resolve_receiver(viewer_agent_id, input.receiver, ctx)
            .await?;

        // Self-message would create a one-party session: representationally
        // invalid (CLAUDE.md §1).
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

        Ok((content, input.context_summary, receiver))
    }

    /// Resolve the wire-side receiver into a [`Participant`]. The agent
    /// branch hits the store for case-insensitive name lookup scoped to
    /// the caller's org; the human branch is direct. NotFound is the
    /// model's fault (`InvalidInput`); a DB-level failure is
    /// infrastructure (`Backend`).
    async fn resolve_receiver(
        &self,
        viewer: crate::agents::AgentId,
        raw: SendMessageReceiver,
        ctx: &ToolCallContext,
    ) -> Result<Participant, ToolError> {
        let name = match raw {
            SendMessageReceiver::Colleague { id } => {
                return self.resolve_colleague(id, ctx).await;
            }
            SendMessageReceiver::Human => {
                // The root human's colleague is resolved via `(org_id, acting_user_id)`
                // — the tool context's acting_user is the DAG-root human under
                // whose authority every send_message runs.
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
        // Resolve the agent's colleague_id in its own org. The agent record
        // carries org_id, so we use that — the directory partial-unique on
        // `(org_id, agent_id)` guarantees one row per agent.
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

    /// Resolve a canonical `{"kind":"colleague","id":…}` receiver into a
    /// colleague-backed [`Participant`].
    ///
    /// The directory read is privileged (it joins `users`, REVOKEd from the app
    /// role), so org isolation is enforced *here*: a colleague outside the
    /// caller's org is reported as unknown rather than leaked. A `System`-style
    /// id can never arrive — System is the NULL convention, never a row, so an
    /// unknown id simply fails the read. Authority is unchanged (locked decision
    /// #4): the session still runs under the agent's `created_by_user_id`; only
    /// *addressing* moves onto the colleague axis.
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

        // Org isolation — the privileged read crosses tenants, so reject a
        // foreign-org colleague as unknown (no existence leak).
        if colleague.org_id() != ctx.org_id {
            set_outcome("unknown_colleague");
            warn!(patom.colleague.id = %id, "send_message.colleague_cross_org");
            return Err(ToolError::InvalidInput(format!(
                "send_message: unknown colleague {id}"
            )));
        }

        match colleague.kind() {
            ColleagueKind::Agent => {
                let agent_id = colleague.agent_id().ok_or_else(|| {
                    set_outcome("backend_error");
                    ToolError::Backend("send_message: agent colleague missing agent_id".to_string())
                })?;
                Ok(Participant::agent(id, agent_id))
            }
            ColleagueKind::Human => {
                let user_id = colleague.user_id().ok_or_else(|| {
                    set_outcome("backend_error");
                    ToolError::Backend("send_message: human colleague missing user_id".to_string())
                })?;
                Ok(Participant::human(id, user_id))
            }
        }
    }

    /// Opening framing — only on a freshly-minted receiver session, and only
    /// when `summary` is non-empty after trim. "Freshly minted" is detected
    /// by checking whether the session already has any messages.
    #[allow(clippy::too_many_arguments)]
    async fn maybe_append_opening_note(
        &self,
        receiver_session: SessionId,
        receiver: Participant,
        viewer: Participant,
        summary: &str,
        request_id: PromptRequestId,
        acting_user_id: crate::auth::UserId,
    ) -> Result<(), ToolError> {
        let trimmed = summary.trim();
        if trimmed.is_empty() {
            return Ok(());
        }
        // §6: receiver is never System here (System is rejected upstream).
        let receiver_colleague = receiver.colleague_id().ok_or_else(|| {
            ToolError::Backend("send_message: opening-note receiver missing colleague".to_string())
        })?;
        let snapshot = self
            .sessions
            .snapshot(receiver_session, receiver_colleague)
            .await
            .map_err(|e| ToolError::Backend(format!("send_message: snapshot failed: {e}")))?;
        if !snapshot.is_empty() {
            return Ok(());
        }
        self.sessions
            .append_system_nudge_for_user(
                acting_user_id,
                receiver_session,
                receiver,
                format!("[context from {viewer}] {trimmed}"),
                request_id,
            )
            .await
            .map_err(|e| {
                ToolError::Backend(format!("send_message: opening note append failed: {e}"))
            })
    }

    // One span per call. `patom.send_message.outcome` is recorded on
    // every exit path so dashboards can `GROUP BY` it without joining
    // through events. The receiver kind / id and DAG root are known
    // before validation; receiver_session lands once the resolve hits.
    #[tracing::instrument(
        skip_all,
        name = "tool.send_message",
        fields(
            patom.dag.root = %ctx.root_request_id,
            patom.session.id = %ctx.session_id,
            patom.from.viewer = %ctx.viewer,
            patom.send_message.outcome = tracing::field::Empty,
            patom.receiver.session.id = tracing::field::Empty,
        ),
    )]
    #[allow(clippy::too_many_lines)] // straight-line dispatch with branches per receiver kind / error
    async fn handle(
        &self,
        input: SendMessageInput,
        ctx: &ToolCallContext,
    ) -> Result<SendMessageOutput, ToolError> {
        let (content, summary, receiver) = self.validate(input, ctx).await?;

        // Tenancy: a child / sibling session inherits the caller's
        // session's `(org_id, created_by_user_id)`. The trigger on
        // `sessions` rejects any cross-org parent/child fork, so this is
        // both the right value and the only value the DB will accept.
        let tenancy = self.sessions.tenancy(ctx.session_id).await.map_err(|e| {
            set_outcome("session_resolve_failed");
            ToolError::Backend(format!("send_message: tenancy lookup failed: {e}"))
        })?;

        // Resolve-or-create the receiver session, parented to the caller's
        // current session. Same path for both branches: an Agent receiver
        // hits an existing or fresh sibling; a Human receiver hits the root
        // session(human, caller_agent) or, for descendant agents that have
        // not yet messaged the human, a freshly-minted (Human, Agent(X))
        // session in the same DAG.
        let caller = crate::auth::Caller::new(ctx.acting_user_id, tenancy.org_id);
        let receiver_session = self
            .sessions
            .resolve_or_create_for_pair_for_user(
                &caller,
                ctx.root_request_id,
                ctx.viewer,
                receiver,
                Some(ctx.session_id),
            )
            .await
            .map_err(|e| {
                set_outcome("session_resolve_failed");
                ToolError::Backend(format!("send_message: session resolve failed: {e}"))
            })?;
        tracing::Span::current().record(
            "patom.receiver.session.id",
            tracing::field::display(receiver_session),
        );

        if let Some(s) = summary.as_deref() {
            self.maybe_append_opening_note(
                receiver_session,
                receiver,
                ctx.viewer,
                s,
                ctx.request_id,
                ctx.acting_user_id,
            )
            .await
            .inspect_err(|_| set_outcome("opening_note_failed"))?;
        }

        // Append the outbound message ONLY for human receivers, and only
        // when the receiver session is *different* from the caller's. For
        // agent receivers the worker's `agent.reply` re-appends the prompt
        // when it claims the queued row; for human receivers in the same
        // session (the common human↔agent root case, where
        // `resolve_or_create_for_pair` returns the existing session) the
        // caller's own `Assistant([…, ToolCall])` row already persisted by
        // `turn.rs` carries the message text. In both cases an extra append
        // here creates a row whose viewer-mapped form (`is_self=true` ⇒
        // `Assistant.text`) lands between the caller's `Assistant.tool_calls`
        // and the matching `Tool` reply on the next turn's wire payload —
        // which OpenAI rejects. The cross-session human path (e.g. a
        // descendant agent first reaching the human) still needs the append
        // because the caller's tool_call lives in a different session.
        if receiver.is_human() && receiver_session != ctx.session_id {
            self.sessions
                .append_for_user(
                    ctx.acting_user_id,
                    receiver_session,
                    MessageSender::from_participant(ctx.viewer),
                    receiver,
                    outbound_chat_message(content.as_str()),
                    ctx.request_id,
                )
                .await
                .map_err(|e| {
                    set_outcome("append_failed");
                    ToolError::Backend(format!("send_message: append failed: {e}"))
                })?;
        }

        // Branch on receiver kind. Human delivery publishes on the root
        // request's stream and is non-blocking (no queue row). Agent
        // delivery enqueues a `prompt_requests` row for the worker.
        //
        // The DAG turn budget bounds *agent* turns spawned within a DAG, so
        // only the agent branch bumps it — a message to a human creates no turn
        // and must not consume the loop budget. The bump runs before the
        // enqueue so an over-budget DAG is rejected without minting a row.
        match receiver {
            Participant::Human { .. } => {
                self.publish_to_human(ctx, receiver_session, content.as_str())
                    .await
            }
            Participant::Agent { agent_id, .. } => {
                self.bump_dag_budget(ctx).await?;
                self.enqueue_for_agent(ctx, receiver_session, agent_id, content, tenancy)
                    .await
            }
            Participant::System => {
                set_outcome("invalid_input");
                Err(ToolError::InvalidInput(ERR_SYSTEM_RECEIVER.into()))
            }
        }
    }

    /// Atomically bump the DAG turn budget for an agent-spawning delivery.
    ///
    /// On exceed we intentionally do not roll the appended rows back, so the
    /// caller sees exactly which message broke the budget; the atomic bump means
    /// two concurrent callers cannot both squeeze past the cap. Only the agent
    /// branch calls this — humans don't consume an agent turn.
    async fn bump_dag_budget(&self, ctx: &ToolCallContext) -> Result<(), ToolError> {
        match self
            .dag
            .bump_or_fail_for_user(ctx.acting_user_id, ctx.root_request_id)
            .await
        {
            Ok(bumped) => {
                debug!(
                    patom.dag.turns_used = bumped.turns_used,
                    patom.dag.turns_cap = bumped.turns_cap,
                    "send_message.dag.bump",
                );
                Ok(())
            }
            Err(e @ PromptError::DagBudgetExceeded { .. }) => {
                set_outcome("dag_exceeded");
                warn!(error = %e, patom.dag.root = %ctx.root_request_id, "send_message.dag.exceeded");
                // Surface the rejection as a terminal failure on the root
                // request's stream so the SSE client learns the DAG hit its
                // loop budget without waiting for quiescence to drain. Best
                // effort — a missing root stream (test-only synthetic root)
                // surfaces as a benign NotFound and is dropped.
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

    /// Publish an [`ResponseChunk::AgentMessage`] on the *current claim's*
    /// request stream so the human SSE client sees the agent's reply.
    /// Non-terminal — the terminal `Done` chunk fires only on DAG quiescence
    /// (`Worker::maybe_emit_quiescence`).
    ///
    /// The chunk is published on `ctx.request_id` (the row whose sink is
    /// open right now) rather than `ctx.root_request_id` — the latter can
    /// point at a long-quiesced first-prompt sink in a continuing thread,
    /// where this publish would fail with "stream already closed". Postgres
    /// `LISTEN/NOTIFY` then routes the chunk by `prompt_requests.root_request_id`
    /// to the right `/threads/{root}/stream` fan-in, so the user's UI sees
    /// the chunk regardless of which prompt it was published on.
    async fn publish_to_human(
        &self,
        ctx: &ToolCallContext,
        receiver_session: SessionId,
        content: &str,
    ) -> Result<SendMessageOutput, ToolError> {
        // The viewer is always Agent here (validate enforced it); pull its
        // id so the chunk records *which* agent authored the message.
        let from = ctx
            .viewer
            .agent_id()
            .expect("invariant: validate() rejects non-agent callers");
        let chunk = ResponseChunk::AgentMessage {
            from,
            to_session: receiver_session,
            content: content.to_string(),
        };
        if let Err(e) = self
            .sink
            .publish_for_user(ctx.acting_user_id, ctx.request_id, chunk)
            .await
        {
            set_outcome("publish_failed");
            warn!(
                error = %e,
                patom.request.id = %ctx.request_id,
                patom.dag.root = %ctx.root_request_id,
                "send_message.publish.error",
            );
            return Err(ToolError::Backend(format!(
                "send_message: publish to human failed: {e}"
            )));
        }
        set_outcome("human_delivered");
        info!(
            patom.from.agent.id = %from,
            text.preview = %preview(content),
            "send_message.delivered_to_human",
        );
        Ok(SendMessageOutput {
            session_id: receiver_session,
            request_id: None,
            delivery: "published",
        })
    }

    /// Enqueue a `prompt_requests` row for the receiving agent. Worker
    /// resolves the agent from the registry and runs the turn. Idempotency
    /// key is derived from the `(caller_session, receiver_session, content)`
    /// triple so a model retry on the same text doesn't duplicate the row.
    async fn enqueue_for_agent(
        &self,
        ctx: &ToolCallContext,
        receiver_session: SessionId,
        receiver_agent_id: AgentId,
        content: Prompt,
        tenancy: crate::session::SessionTenancy,
    ) -> Result<SendMessageOutput, ToolError> {
        let key = idempotency_key(ctx, receiver_session, content.as_str());
        let key = IdempotencyKey::try_from(key).map_err(|e| {
            // We constructed the key — a parse failure is a programmer
            // error, never the model's fault.
            ToolError::Backend(format!("send_message: bad idempotency: {e}"))
        })?;
        let preview_str = preview(content.as_str());
        // Capture the body before the queue takes ownership — the
        // visibility publish below re-surfaces it as an `AgentMessage`.
        let visible_content = content.as_str().to_owned();
        let from = ctx
            .viewer
            .agent_id()
            .expect("invariant: validate() rejects non-agent callers");
        let outcome = self
            .queue
            .enqueue_for_user(
                ctx.acting_user_id,
                NewPromptRequest::normal(
                    Some(receiver_session),
                    ctx.viewer,
                    receiver_agent_id,
                    Some(ctx.session_id),
                    content,
                    key,
                    tenancy.org_id,
                    tenancy.created_by_user_id,
                ),
            )
            .await
            .map_err(|e| {
                set_outcome("enqueue_failed");
                ToolError::Backend(format!("send_message: enqueue failed: {e}"))
            })?;

        // Surface the handoff to stream observers (Slack pump, web SSE) so an
        // agent↔agent exchange is visible just like an agent→human reply. The
        // queue row above is the authoritative delivery; this publish is
        // observation only, so a failure must not fail the tool.
        self.publish_agent_visibility(ctx, from, receiver_session, visible_content)
            .await;

        set_outcome("agent_delivered");
        info!(
            patom.request.id = %outcome.request_id(),
            patom.from.agent.id = %from,
            patom.to.agent.id = %receiver_agent_id,
            text.preview = %preview_str,
            "send_message.delivered",
        );

        Ok(SendMessageOutput {
            session_id: receiver_session,
            request_id: Some(outcome.request_id()),
            delivery: "queued",
        })
    }

    /// Best-effort visibility publish for an agent→agent delivery.
    ///
    /// Emits the same [`ResponseChunk::AgentMessage`] shape an agent→human
    /// reply produces, keyed to the agent-pair `receiver_session`. The Slack
    /// stream pump routes it by `to_session` and mints a per-pair thread, so
    /// the human watching the channel sees the agents talk. Published on
    /// `ctx.request_id` (the caller's open claim sink) for the same reason
    /// [`Self::publish_to_human`] is — see that method's note.
    ///
    /// Unlike `publish_to_human`, the authoritative delivery is the enqueued
    /// queue row that drives the receiver's turn; a publish failure here is
    /// logged and swallowed rather than surfaced as a tool error.
    async fn publish_agent_visibility(
        &self,
        ctx: &ToolCallContext,
        from: AgentId,
        to_session: SessionId,
        content: String,
    ) {
        let chunk = ResponseChunk::AgentMessage {
            from,
            to_session,
            content,
        };
        if let Err(e) = self
            .sink
            .publish_for_user(ctx.acting_user_id, ctx.request_id, chunk)
            .await
        {
            warn!(
                error = %e,
                patom.request.id = %ctx.request_id,
                patom.dag.root = %ctx.root_request_id,
                "send_message.agent_visibility.publish_failed",
            );
        }
    }
}

/// Record the `patom.send_message.outcome` field on the enclosing
/// `tool.send_message` span. Each decision point in [`SendMessageTool::handle_inner`]
/// labels its branch before returning so dashboards can `GROUP BY
/// patom.send_message.outcome` without joining through events. Variants:
/// `agent_delivered`, `human_delivered`, `dag_exceeded`, `dag_failed`,
/// `unknown_agent`, `invalid_input`, `backend_error`,
/// `session_resolve_failed`, `opening_note_failed`, `append_failed`,
/// `publish_failed`, `enqueue_failed`.
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

/// Render the outbound message as a single text-block user-content row.
/// Stored under the caller's MessageSender, so the viewer-mapped snapshot
/// renders it as Assistant for the caller and User for the receiver — that
/// shape is exactly what the chat-completion provider expects.
fn outbound_chat_message(content: &str) -> crate::provider::ChatMessage {
    use crate::provider::{ChatMessage, UserContent};
    ChatMessage::User(vec![UserContent::Text(content.to_string())])
}

/// Stable idempotency key for a `send_message` call. The triple
/// `(caller_session, receiver_session, content_hash)` keeps two retries of
/// the same text from creating two queue rows while still allowing distinct
/// payloads through. Hash is FNV-1a-64 over the content bytes — deterministic
/// across processes (the queue lookup is exact-match, so two replicas must
/// agree on the same key for the same payload). Collisions don't break
/// correctness, only dedup precision.
fn idempotency_key(ctx: &ToolCallContext, receiver_session: SessionId, content: &str) -> String {
    format!(
        "send-msg:{}:{}:{:016x}",
        ctx.session_id.as_uuid(),
        receiver_session.as_uuid(),
        fnv1a64(content.as_bytes()),
    )
}

/// FNV-1a 64-bit hash. Tiny, deterministic, no dependency cost.
fn fnv1a64(bytes: &[u8]) -> u64 {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut h = OFFSET;
    for b in bytes {
        h ^= u64::from(*b);
        h = h.wrapping_mul(PRIME);
    }
    h
}
