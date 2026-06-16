//! Per-turn execution: provider call, tool calls, request assembly.
//!
//! Lifecycle (`reply` / `resume` / `run_loop`) lives in [`super::core`]; this
//! module owns the body of one iteration of the turn loop.

use std::time::Duration as StdDuration;

use chrono::{DateTime, Utc};
use tokio::time::{Instant, timeout};
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

use crate::auth::Caller;
use crate::background::{BackgroundTurnId, NewBackgroundMessage};
use crate::hook::{ToolContext, TurnContext};
use crate::provider::{
    ChatMessage, ChatRequest, ChatResponse, SharedProvider, StopReason, ToolCall, ToolCallId,
    ToolResult, UserContent,
};
use crate::runtime::{
    IdempotencyKey, MetricKind, PromptRequestId, RequestKind, RequestKindPayload,
};
use crate::threads::{AgentThreadId, MessageKind, NewMessage, ThreadId};
use crate::tools::{
    SharedTool, TOOL_RESULT_MAX_BYTES, ToolBox, ToolCallContext, ToolCallRow, ToolCallRowId,
    clip_error_message, truncate_to_char_boundary,
};
use crate::types::{Participant, TurnIndex};

use crate::auth::OrgId;
use crate::billing::{BillingError, price_for, turn_cost};

use super::core::{Agent, send_message_tool_name};
use super::error::AgentError;
use super::limits::MAX_TOOL_CALLS_PER_TURN;
use super::log;
use super::observer::SharedTurnObserver;
use super::turn_metrics::{
    DurationMs, InputTokens, OutputTokens, StopReasonLabel, TurnMetricsId, TurnMetricsRow,
};

impl Agent {
    /// Run one thread-feed turn: build the request from the feed (read-at-run),
    /// call the provider, append the agent's artifacts back to the feed. Returns
    /// `Some(text)` when the turn ends with a final answer; `None` to continue.
    ///
    /// The assistant turn and its tool results are appended as **owner-private**
    /// feed rows (shown to all, ingested only by this agent next turn). The
    /// posted egress to peers is the `send_message` tool, not this raw
    /// completion.
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn run_thread_turn(
        &self,
        ctx: TurnContext,
        claim_key: AgentThreadId,
        thread: ThreadId,
        viewer: Participant,
        request_id: PromptRequestId,
        root_request_id: PromptRequestId,
        caller: Caller,
        kind_payload: &RequestKindPayload,
        send_message_calls: &mut usize,
        cancel: &CancellationToken,
        observer: Option<&SharedTurnObserver>,
    ) -> Result<Option<String>, AgentError> {
        self.hooks().before_turn(ctx).await?.into_result()?;
        // Resolve the provider client + BYO flag once for this turn from one
        // overlay snapshot, so the gate, the send, and the settle all agree
        // even if the overlay swaps mid-turn (#141).
        let (provider, is_byo) = self.route();
        self.billing_gate(caller.org_id, is_byo).await?;
        let started_at = self.clock().now_utc();
        let started_mono = Instant::now();
        // An inline compaction reuses this turn's provider and meters under its
        // `request_id` (#182). The turn's billing gate above covers its folds.
        let routing = super::compaction::TurnRouting {
            provider: &provider,
            request_id,
            org: caller.org_id,
        };
        let request = self
            .build_thread_request(claim_key, thread, viewer, kind_payload, &routing)
            .await?;
        let response = self
            .call_provider(&provider, request, self.provider_timeout(), cancel)
            .await?;
        let duration = started_mono.elapsed();
        self.record_turn_metrics(
            request_id,
            Some(claim_key),
            caller.org_id,
            kind_payload.kind().into(),
            started_at,
            duration,
            &response,
        )
        .await;
        self.billing_settle(caller.org_id, request_id, ctx.turn_index, &response, is_byo)
            .await;
        self.hooks()
            .after_turn(ctx, &response)
            .await?
            .into_result()?;

        for block in &response.content {
            log::assistant_block(ctx.turn_index.get(), block);
        }
        if let Some(obs) = observer {
            for block in &response.content {
                obs.on_assistant(block).await;
            }
        }

        // §6: the thread path only ever runs for an agent viewer.
        let agent_id = viewer.agent_id().ok_or_else(|| {
            AgentError::Internal("run_thread_turn requires an agent viewer".to_string())
        })?;

        let tool_calls = response.tool_calls();
        tracing::Span::current().record("patom.tool_calls.count", tool_calls.len());

        // The assistant turn → an owner-private artifact. `ToolUse` when the
        // turn issued tool calls, else `Reasoning`; either way owner-scoped so
        // only this agent re-ingests it (peers see it for transparency).
        let assistant_kind = if tool_calls.is_empty() {
            MessageKind::Reasoning
        } else {
            MessageKind::ToolUse
        };
        self.append_private(
            &caller,
            thread,
            assistant_kind,
            viewer.colleague_id(),
            agent_id,
            ChatMessage::Assistant(response.content.clone()),
            request_id,
        )
        .await?;

        if tool_calls.is_empty() {
            let text = response.text();
            if text.is_empty() {
                return Err(AgentError::EmptyReply);
            }
            return Ok(Some(text));
        }
        if tool_calls.len() > MAX_TOOL_CALLS_PER_TURN {
            return Err(AgentError::TooManyToolCalls {
                max: MAX_TOOL_CALLS_PER_TURN,
            });
        }

        self.run_thread_tool_calls(
            ctx,
            claim_key,
            thread,
            viewer,
            agent_id,
            request_id,
            root_request_id,
            caller,
            kind_payload,
            &tool_calls,
            send_message_calls,
            cancel,
            observer,
        )
        .await?;
        Ok(None)
    }

    /// Append one **owner-private** feed artifact (reasoning / tool_use /
    /// tool_result): shown to everyone in the thread but ingested only by
    /// `owner` on its next turn. Thin wrapper over [`crate::threads::ThreadStore::append`]
    /// so the two call sites in [`Self::run_thread_turn`] stay terse.
    #[allow(clippy::too_many_arguments)]
    async fn append_private(
        &self,
        caller: &Caller,
        thread: ThreadId,
        kind: MessageKind,
        sender: Option<crate::colleagues::ColleagueId>,
        owner: crate::agents::AgentId,
        body: ChatMessage,
        request_id: PromptRequestId,
    ) -> Result<(), AgentError> {
        self.threads()
            .append(
                caller,
                thread,
                NewMessage {
                    kind,
                    sender,
                    owner_agent_id: Some(owner),
                    receiver: None,
                    body,
                    request_id: Some(request_id),
                    idempotency_key: None,
                },
            )
            .await?;
        Ok(())
    }

    /// Run the assistant turn's tool calls and append their results as an
    /// owner-private `tool_result` artifact. Counts `send_message` attempts for
    /// the worker's ping-pong guard regardless of per-call error.
    #[allow(clippy::too_many_arguments)]
    async fn run_thread_tool_calls(
        &self,
        ctx: TurnContext,
        claim_key: AgentThreadId,
        thread: ThreadId,
        viewer: Participant,
        agent_id: crate::agents::AgentId,
        request_id: PromptRequestId,
        root_request_id: PromptRequestId,
        caller: Caller,
        kind_payload: &RequestKindPayload,
        tool_calls: &[&ToolCall],
        send_message_calls: &mut usize,
        cancel: &CancellationToken,
        observer: Option<&SharedTurnObserver>,
    ) -> Result<(), AgentError> {
        // `claim_key` is the polymorphic turn scope (here the participation id);
        // `state_id` carries the same id as the recorder FK. `root_request_id`
        // is the real DAG root (resolved by the worker), so `send_message`'s
        // budget bump lands on the right `prompt_request_dags`.
        let tool_ctx = ToolCallContext {
            claim_key: ctx.claim_key,
            thread_id: Some(thread),
            state_id: Some(claim_key),
            viewer,
            root_request_id,
            request_id,
            kind_payload: kind_payload.clone(),
            acting_user_id: caller.user_id,
            org_id: caller.org_id,
        };
        for call in tool_calls {
            if call.name.as_str() == send_message_tool_name() {
                *send_message_calls += 1;
            }
        }
        let results = self
            .run_tools(
                ctx,
                tool_calls,
                self.tools(),
                kind_payload.kind(),
                &tool_ctx,
                cancel,
                observer,
            )
            .await?;
        self.append_private(
            &caller,
            thread,
            MessageKind::ToolResult,
            None,
            agent_id,
            ChatMessage::User(results.into_iter().map(UserContent::ToolResult).collect()),
            request_id,
        )
        .await
    }

    /// Assemble a thread-feed turn's provider request: the agent's feed context
    /// (read-at-run, viewer-mapped) + the thread system prompt + tool specs.
    #[tracing::instrument(
        skip_all,
        name = "thread.context.build",
        fields(
            patom.thread.id = %thread,
            patom.viewer = %viewer,
            patom.history.count = tracing::field::Empty,
            patom.system_prompt.bytes = tracing::field::Empty,
        ),
    )]
    async fn build_thread_request(
        &self,
        state_id: AgentThreadId,
        thread: ThreadId,
        viewer: Participant,
        kind_payload: &RequestKindPayload,
        routing: &super::compaction::TurnRouting<'_>,
    ) -> Result<ChatRequest, AgentError> {
        let span = tracing::Span::current();
        let agent_id = viewer.agent_id().ok_or_else(|| {
            AgentError::Internal("build_thread_request requires an agent viewer".to_string())
        })?;
        let viewer_colleague = viewer.colleague_id().ok_or_else(|| {
            AgentError::Internal("build_thread_request agent viewer has no colleague".to_string())
        })?;
        // Resolve per-platform display labels (e.g. Slack handles) once, so
        // the feed (sender attribution) and the system prompt (roster) name
        // people identically. Empty for web/background threads.
        let overrides = self.memory().display_overrides(Some(thread)).await;
        // The bounded-context assembly (windowing floor + rolling summary, #182),
        // the system-prompt compose, and the thread's participant roll-up hit
        // independent stores; run all three concurrently so the turn pays one
        // round-trip latency, not three.
        let (ctx, memory_system, participants) = tokio::join!(
            self.resolve_agent_context(
                thread,
                agent_id,
                viewer_colleague,
                &overrides,
                state_id,
                routing,
            ),
            self.memory()
                .system_prompt_for_thread(viewer, &overrides, kind_payload),
            self.threads().thread_participants(thread),
        );
        let ctx = ctx?;
        let memory_system = memory_system?;
        let messages = ctx.messages;
        assert!(
            !messages.is_empty(),
            "thread turn must read at least one feed message"
        );
        // The windowing floor — bounded regardless of summary state (CLAUDE.md §6).
        let context_cap =
            usize::try_from(crate::threads::MAX_CONTEXT_MESSAGES).unwrap_or(usize::MAX);
        assert!(
            messages.len() <= context_cap,
            "context tail must respect the windowing floor"
        );
        span.record("patom.history.count", messages.len());

        // L1 + L2: the `<participants>` block — who raised the thread and who
        // has posted, enriched with their shared profiles. The participant read
        // above overlapped the feed/prompt reads; only the (cache-warm) render
        // happens here. A thread-store outage degrades to empty (enrichment, not
        // load-bearing); the block render degrades independently.
        let participants_block = match participants {
            Ok(participants) => {
                self.memory()
                    .participants_block(&participants, viewer_colleague, &overrides)
                    .await
            }
            Err(e) => {
                tracing::warn!(error = %e, "thread.participants.error");
                String::new()
            }
        };

        // Fold the agent's per-thread todo list (keyed on `state_id`, the
        // participation id) into the system-prompt tail. Empty / missing-store
        // cases render to the empty string, so `format!` leaves no trailing
        // separator. `TodoWriteTool` writes the same `state_id`.
        let todos_block = match self.todos_store() {
            Some(store) => {
                let list =
                    tokio::time::timeout(super::limits::TODOS_LOAD_TIMEOUT, store.get(state_id))
                        .await
                        .map_err(|_| AgentError::TodosLoadTimeout)??;
                crate::tools::system::todos::render_section(&list)
            }
            None => String::new(),
        };
        // The per-turn tail blocks sit after `<memory>`, so they never perturb
        // the org-stable prefix that drives prompt-cache hits: the `<participants>`
        // block, then the todo list, then the rolling compaction summary (#182).
        let system: std::sync::Arc<str> = if participants_block.is_empty()
            && todos_block.is_empty()
            && ctx.summary.is_none()
        {
            memory_system // zero-copy fast path
        } else {
            let mut s = memory_system.to_string();
            if !participants_block.is_empty() {
                s.push('\n');
                s.push_str(&participants_block);
            }
            if !todos_block.is_empty() {
                s.push('\n');
                s.push_str(&todos_block);
            }
            if let Some(summary) = &ctx.summary {
                s.push_str("\n\n## Earlier conversation (compacted)\n");
                s.push_str(summary.as_str());
            }
            std::sync::Arc::from(s.as_str())
        };
        span.record("patom.system_prompt.bytes", system.len());

        let tools = self.tools().specs_for(kind_payload.kind());
        Ok(ChatRequest {
            model: self.model(),
            system,
            messages,
            tools,
            max_output_tokens: self.max_output_tokens(),
        })
    }

    /// Run one background-cognition turn: build the request from the turn's
    /// private log, call the provider, append the agent's exchange back to the
    /// background store. No chat-feed rows, no ping-pong (cognition may end
    /// without `send_message`). Returns `Some(text)` on a final answer.
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn run_background_turn(
        &self,
        ctx: TurnContext,
        turn: BackgroundTurnId,
        viewer: Participant,
        request_id: PromptRequestId,
        caller: Caller,
        kind_payload: &RequestKindPayload,
        send_message_calls: &mut usize,
        cancel: &CancellationToken,
        observer: Option<&SharedTurnObserver>,
    ) -> Result<Option<String>, AgentError> {
        self.hooks().before_turn(ctx).await?.into_result()?;
        let (provider, is_byo) = self.route();
        self.billing_gate(caller.org_id, is_byo).await?;
        let started_at = self.clock().now_utc();
        let started_mono = Instant::now();
        let request = self
            .build_background_request(turn, viewer, caller, kind_payload)
            .await?;
        let response = self
            .call_provider(&provider, request, self.provider_timeout(), cancel)
            .await?;
        let duration = started_mono.elapsed();
        // Background turns have no `agent_thread_state` row, so the recorder FK
        // would fail — skip recording (state_id = None) for cognition turns.
        self.record_turn_metrics(
            request_id,
            None,
            caller.org_id,
            kind_payload.kind().into(),
            started_at,
            duration,
            &response,
        )
        .await;
        self.billing_settle(caller.org_id, request_id, ctx.turn_index, &response, is_byo)
            .await;
        self.hooks()
            .after_turn(ctx, &response)
            .await?
            .into_result()?;

        for block in &response.content {
            log::assistant_block(ctx.turn_index.get(), block);
        }
        if let Some(obs) = observer {
            for block in &response.content {
                obs.on_assistant(block).await;
            }
        }

        // §6: the background path only ever runs for an agent viewer.
        let _agent_id = viewer.agent_id().ok_or_else(|| {
            AgentError::Internal("run_background_turn requires an agent viewer".to_string())
        })?;
        // The assistant turn → the turn's private log (never the chat feed).
        self.append_background(
            &caller,
            turn,
            viewer.colleague_id(),
            ChatMessage::Assistant(response.content.clone()),
            request_id,
        )
        .await?;

        let tool_calls = response.tool_calls();
        tracing::Span::current().record("patom.tool_calls.count", tool_calls.len());
        if tool_calls.is_empty() {
            let text = response.text();
            if text.is_empty() {
                return Err(AgentError::EmptyReply);
            }
            return Ok(Some(text));
        }
        if tool_calls.len() > MAX_TOOL_CALLS_PER_TURN {
            return Err(AgentError::TooManyToolCalls {
                max: MAX_TOOL_CALLS_PER_TURN,
            });
        }

        // Cognition tools (memory write/update/forget/validate, contradiction
        // close) run with thread/state unset; `claim_key` is the background
        // turn id (the polymorphic turn scope) and `state_id` is `None` so no
        // recorder FK is attempted.
        let tool_ctx = ToolCallContext {
            claim_key: ctx.claim_key,
            thread_id: None,
            state_id: None,
            viewer,
            root_request_id: request_id,
            request_id,
            kind_payload: kind_payload.clone(),
            acting_user_id: caller.user_id,
            org_id: caller.org_id,
        };
        for call in &tool_calls {
            if call.name.as_str() == send_message_tool_name() {
                *send_message_calls += 1;
            }
        }
        let results = self
            .run_tools(
                ctx,
                &tool_calls,
                self.tools(),
                kind_payload.kind(),
                &tool_ctx,
                cancel,
                observer,
            )
            .await?;
        self.append_background(
            &caller,
            turn,
            None,
            ChatMessage::User(results.into_iter().map(UserContent::ToolResult).collect()),
            request_id,
        )
        .await?;
        Ok(None)
    }

    /// Append one row to a background turn's private log (assistant turn or tool
    /// results). Thin wrapper so [`Self::run_background_turn`] stays terse.
    async fn append_background(
        &self,
        caller: &Caller,
        turn: BackgroundTurnId,
        sender: Option<crate::colleagues::ColleagueId>,
        body: ChatMessage,
        request_id: PromptRequestId,
    ) -> Result<(), AgentError> {
        self.background()
            .append(
                caller,
                turn,
                NewBackgroundMessage {
                    sender,
                    body,
                    request_id: Some(request_id),
                },
            )
            .await?;
        Ok(())
    }

    /// Assemble a background turn's provider request: the turn's private log
    /// (read-at-run) + the kind-specific system prompt + tool specs.
    #[tracing::instrument(
        skip_all,
        name = "background.context.build",
        fields(patom.background.turn.id = %turn, patom.history.count = tracing::field::Empty),
    )]
    async fn build_background_request(
        &self,
        turn: BackgroundTurnId,
        viewer: Participant,
        caller: Caller,
        kind_payload: &RequestKindPayload,
    ) -> Result<ChatRequest, AgentError> {
        let messages = self.background().context(&caller, turn).await?;
        assert!(
            !messages.is_empty(),
            "background turn must have a seeded prompt to read"
        );
        tracing::Span::current().record("patom.history.count", messages.len());
        // Background cognition has no feed/thread → no platform labels.
        let system = self
            .memory()
            .system_prompt_for_thread(viewer, &std::collections::HashMap::new(), kind_payload)
            .await?;
        let tools = self.tools().specs_for(kind_payload.kind());
        Ok(ChatRequest {
            model: self.model(),
            system,
            messages,
            tools,
            max_output_tokens: self.max_output_tokens(),
        })
    }

    /// Pre-turn spend gate. Returns [`AgentError::BillingExceeded`] when the org
    /// is at/over its monthly cap. A DB error fails *open* — a transient blip
    /// must not block a turn the admission gate already admitted; the counter is
    /// reconciled from `turn_metrics`. No-op when no budget service is wired
    /// (agent_core unit tests).
    ///
    /// `is_byo` short-circuits the gate entirely (#141): a turn routed through
    /// the org's own provider key is the org's own spend, so a zero platform
    /// balance (or an exhausted monthly cap) must never block it.
    async fn billing_gate(&self, org: OrgId, is_byo: bool) -> Result<(), AgentError> {
        if is_byo {
            return Ok(());
        }
        let Some(budget) = self.billing() else {
            return Ok(());
        };
        match budget.check_or_fail(org).await {
            Ok(()) => Ok(()),
            Err(BillingError::Exceeded { .. }) => Err(AgentError::BillingExceeded { org }),
            Err(BillingError::OutOfCredit { .. }) => Err(AgentError::OutOfCredit { org }),
            Err(BillingError::Db(e)) => {
                tracing::error!(
                    error = ?e,
                    patom.org.id = %org,
                    "billing.gate.db_error_fail_open",
                );
                Ok(())
            }
        }
    }

    /// Post-paid settle: add this turn's actual cost to the org's current
    /// period. Fail-open — a settle failure is logged and the turn proceeds (the
    /// user already received the answer); `turn_metrics` is the reconciliation
    /// ledger (CLAUDE.md §6). No-op when no budget service is wired.
    ///
    /// `is_byo` skips settle entirely (#141): a BYO turn is the org's own spend,
    /// so it neither debits platform credit nor counts toward the monthly
    /// platform cap. `turn_metrics` (recorded before this call) still captures
    /// its cost for BYO usage analytics.
    async fn billing_settle(
        &self,
        org: OrgId,
        request_id: PromptRequestId,
        turn_index: TurnIndex,
        response: &ChatResponse,
        is_byo: bool,
    ) {
        let cost = turn_cost(price_for(self.model()), &response.usage);
        if is_byo {
            // BYO usage analytics (#141): a parallel signal to `credit.debit`
            // so dashboards can break turns down BYO-vs-platform. No key
            // material (CLAUDE.md §2); the org paid its own provider directly.
            tracing::info!(
                event = "billing.byo_skip",
                patom.org.id = %org,
                patom.provider = self.model().provider().as_str(),
                patom.byo.cost_micro = cost.get(),
            );
            return;
        }
        let Some(budget) = self.billing() else {
            return;
        };
        // Stable per-turn key so a retried settle (worker resume) never
        // double-debits credits: `(request_id, turn_index)` is the same across
        // re-runs of the same turn, unlike the provider's response id.
        let usage_key =
            IdempotencyKey::try_from(format!("usage:{request_id}:{}", turn_index.get())).ok();
        if let Err(e) = budget.settle(org, cost, usage_key.as_ref()).await {
            tracing::error!(
                error = ?e,
                patom.org.id = %org,
                "billing.settle.failed",
            );
        }
    }

    /// Best-effort write to `turn_metrics`. Skipped when the recorder is
    /// not wired (agent_core unit tests) or when `state_id` is `None` (the
    /// background-cognition path has no `agent_thread_state` row, so the FK
    /// would fail). DB / conversion failures emit `tracing::error!` and
    /// continue — the user has already seen the turn (CLAUDE.md §6:
    /// observability never blocks the user-visible path; the row going
    /// missing is one chart cell, not a turn replay).
    #[allow(clippy::too_many_arguments)] // recorder bundles per-call audit fields, not branching
    pub(super) async fn record_turn_metrics(
        &self,
        request_id: PromptRequestId,
        state_id: Option<AgentThreadId>,
        org_id: crate::auth::OrgId,
        kind: MetricKind,
        started_at: chrono::DateTime<chrono::Utc>,
        duration: StdDuration,
        response: &ChatResponse,
    ) {
        let Some(binding) = self.turn_metrics() else {
            return;
        };
        // `turn_metrics.state_id` FKs `agent_thread_state`; cognition turns
        // have no such row, so skip recording rather than FK-fail.
        let Some(state_id) = state_id else {
            return;
        };
        // Token counts come from the provider as `u32`; the newtype's
        // `TryFrom<u32>` enforces fit-in-i32. A counter that wraps the
        // bound is a provider bug — log and skip rather than panic.
        let (Ok(input_tokens), Ok(output_tokens)) = (
            InputTokens::try_from(response.usage.input_tokens),
            OutputTokens::try_from(response.usage.output_tokens),
        ) else {
            tracing::error!(
                patom.request.id = %request_id,
                patom.tokens.input = response.usage.input_tokens,
                patom.tokens.output = response.usage.output_tokens,
                "turn_metrics.skip.token_overflow: provider reported counts that don't fit i32",
            );
            return;
        };
        let cache_creation_tokens = response
            .usage
            .cache_creation_input_tokens
            .and_then(|n| InputTokens::try_from(n).ok());
        let cache_read_tokens = response
            .usage
            .cache_read_input_tokens
            .and_then(|n| InputTokens::try_from(n).ok());
        let row = TurnMetricsRow {
            id: TurnMetricsId::new(),
            request_id,
            org_id,
            state_id,
            agent_id: binding.agent_id,
            prompt_version_id: binding.prompt_version_id,
            kind,
            model: self.model(),
            provider: self.model().provider(),
            input_tokens,
            output_tokens,
            cache_creation_tokens,
            cache_read_tokens,
            duration_ms: DurationMs::saturating_from_millis(duration.as_millis()),
            stop_reason: StopReasonLabel::from_truncated(&stop_reason_label(&response.stop_reason)),
            started_at,
        };
        if let Err(e) = binding.store.record(row).await {
            tracing::error!(
                error = ?e,
                patom.request.id = %request_id,
                "turn_metrics.record.failed",
            );
        }
    }

    /// Single LLM provider entry point. Every code path that talks to a
    /// model — normal turn, reflection, resolution — funnels through here
    /// so timeout, cancellation, and error mapping live in one place.
    pub(super) async fn call_provider(
        &self,
        provider: &SharedProvider,
        request: ChatRequest,
        timeout_after: std::time::Duration,
        cancel: &CancellationToken,
    ) -> Result<ChatResponse, AgentError> {
        // `provider` is the client the turn resolved once via `Agent::route`
        // (#141) — the same overlay snapshot that decided the BYO gate/settle
        // skip, so routing and metering can't disagree.
        let send = provider.send(request);
        tokio::select! {
            biased;
            () = cancel.cancelled() => Err(AgentError::Cancelled),
            r = timeout(timeout_after, send) => match r {
                Ok(Ok(resp)) => Ok(resp),
                Ok(Err(e)) => Err(AgentError::Provider(e)),
                Err(_) => Err(AgentError::ProviderTimeout),
            },
        }
    }

    /// Execute every tool call from the assistant turn against `tools`,
    /// returning a `ToolResult` for each — never short-circuits, so the
    /// model receives a complete picture of what happened. The toolbox is
    /// mode-filtered, so different turn modes (normal, reflection,
    /// resolution) can present different closed sets to the model.
    ///
    /// Consecutive concurrency-safe calls fan out via
    /// [`futures::future::join_all`]; an unsafe (or unknown) call forms
    /// a barrier. `join_all` preserves input order, and tracing /
    /// observer side effects fire in call order from the merge step so
    /// downstream consumers see a deterministic stream.
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn run_tools(
        &self,
        ctx: TurnContext,
        calls: &[&ToolCall],
        tools: &ToolBox,
        kind: RequestKind,
        tool_ctx: &ToolCallContext,
        cancel: &CancellationToken,
        observer: Option<&SharedTurnObserver>,
    ) -> Result<Vec<ToolResult>, AgentError> {
        let classes: Vec<CallClass> = calls
            .iter()
            .map(|c| CallClass::classify(tools.get_for(kind, c.name.as_str())))
            .collect();
        let safe_flags: Vec<bool> = classes.iter().map(CallClass::is_safe).collect();
        let batches = plan_batches(&safe_flags);

        let mut out: Vec<ToolResult> = Vec::with_capacity(calls.len());
        for batch in batches {
            if cancel.is_cancelled() {
                return Err(AgentError::Cancelled);
            }
            // Materialise indices so the merge step can pair each result
            // with its call (the inner future only carries timing).
            let indices: Vec<usize> = batch.collect();
            let results = futures::future::join_all(indices.iter().map(|&i| {
                self.run_call_with_hooks(ctx, calls[i], classes[i].tool(), kind, tool_ctx, cancel)
            }))
            .await;
            // Emit log + observer in call order — `join_all` preserves
            // input order on the result vec, so iterating it is the
            // deterministic merge point. The tool-call recorder fires
            // *after* the observer so a slow insert can't delay the SSE
            // chunk the user is waiting on (CLAUDE.md §6: observability
            // is best-effort, never on the user-visible critical path).
            for (call_idx, r) in indices.into_iter().zip(results) {
                let outcome = r?;
                log::tool_result(ctx.turn_index.get(), &outcome.result);
                if let Some(obs) = observer {
                    obs.on_tool_result(&outcome.result).await;
                }
                self.record_tool_call(tool_ctx, tools, calls[call_idx], &outcome)
                    .await;
                out.push(outcome.result);
            }
        }
        Ok(out)
    }

    /// Best-effort write to the `tool_calls` audit log. Skipped when no
    /// store is wired, the viewer is not an agent (system / human
    /// participants never dispatch tools but the type system can't
    /// prove it here), or `state_id` is `None` (background-cognition turn —
    /// `tool_calls.state_id` FKs `agent_thread_state`, which a cognition
    /// turn has no row in, so the FK would fail). DB failures emit
    /// `tracing::error!` and continue — the user has already seen the result.
    async fn record_tool_call(
        &self,
        tool_ctx: &ToolCallContext,
        tools: &ToolBox,
        call: &ToolCall,
        outcome: &CallOutcome,
    ) {
        let Some(store) = self.tool_call_store() else {
            return;
        };
        let Some(agent_id) = tool_ctx.viewer.agent_id() else {
            return;
        };
        let Some(state_id) = tool_ctx.state_id else {
            return;
        };
        // Carries *what* failed for the audit row, not just *that* it failed.
        let error_message = outcome
            .result
            .is_error
            .then(|| clip_error_message(outcome.result.output.clone()));
        let row = ToolCallRow {
            id: ToolCallRowId::new(),
            org_id: tool_ctx.org_id,
            state_id,
            request_id: tool_ctx.request_id,
            agent_id,
            mcp_server_id: tools.server_id_for(call.name.as_str()),
            tool_name: call.name.clone(),
            started_at: outcome.started_at,
            duration: outcome.duration,
            is_error: outcome.result.is_error,
            error_message,
        };
        if let Err(e) = store.record(row).await {
            tracing::error!(
                error = ?e,
                patom.tool = %call.name,
                "tool_calls.record.failed",
            );
        }
    }

    async fn run_call_with_hooks(
        &self,
        ctx: TurnContext,
        call: &ToolCall,
        tool: Option<SharedTool>,
        kind: RequestKind,
        tool_ctx: &ToolCallContext,
        cancel: &CancellationToken,
    ) -> Result<CallOutcome, AgentError> {
        let hook_ctx = ToolContext {
            claim_key: ctx.claim_key,
            turn_index: ctx.turn_index,
            call,
        };
        self.hooks().before_tool(hook_ctx).await?.into_result()?;
        // Wall-clock `started_at` from the agent's clock (CLAUDE.md §11) so
        // tests can pin timestamps; the monotonic `Instant` runs alongside
        // so paused / faked wall clocks don't zero out `duration`.
        let started_at = self.clock().now_utc();
        let started_mono = Instant::now();
        let result = self.run_one_tool(call, tool, kind, tool_ctx, cancel).await;
        let duration = started_mono.elapsed();
        self.hooks()
            .after_tool(hook_ctx, &result)
            .await?
            .into_result()?;
        Ok(CallOutcome {
            result,
            started_at,
            duration,
        })
    }

    /// Resolve and run a single tool. All failure modes (unknown tool, timeout,
    /// tool error) fold into a `ToolResult { is_error: true }` so the model
    /// can reason about them. Cancellation is the only condition that bubbles.
    #[tracing::instrument(
        skip_all,
        name = "execute_tool",
        fields(
            gen_ai.operation.name = "execute_tool",
            gen_ai.tool.name = %call.name,
            gen_ai.tool.call.id = %call.id.as_str(),
            patom.tool = %call.name,
            patom.claim_key = %tool_ctx.claim_key.as_uuid(),
        ),
    )]
    async fn run_one_tool(
        &self,
        call: &ToolCall,
        tool: Option<SharedTool>,
        kind: RequestKind,
        tool_ctx: &ToolCallContext,
        cancel: &CancellationToken,
    ) -> ToolResult {
        let id = call.id.clone();
        let Some(tool) = tool else {
            warn!(patom.tool = %call.name, "tool.unknown");
            return error_result(
                id,
                format!(
                    "unknown tool for kind={kind}: {name}",
                    kind = kind.as_str(),
                    name = call.name
                ),
            );
        };

        let exec = tool.execute(call.input.clone(), tool_ctx);
        let outcome = tokio::select! {
            biased;
            () = cancel.cancelled() => return error_result(id, "cancelled".into()),
            r = timeout(self.tool_timeout(), exec) => r,
        };

        match outcome {
            Ok(Ok(output)) => {
                if output.len() > TOOL_RESULT_MAX_BYTES {
                    warn!(
                        patom.tool = %call.name,
                        bytes = output.len(),
                        cap = TOOL_RESULT_MAX_BYTES,
                        "tool.result.too_large",
                    );
                    return error_result(
                        id,
                        format!(
                            "tool `{}` returned {} bytes; cap is {} bytes",
                            call.name,
                            output.len(),
                            TOOL_RESULT_MAX_BYTES,
                        ),
                    );
                }
                debug!(patom.tool = %call.name, bytes = output.len(), "tool.result.ok");
                ToolResult {
                    call_id: id,
                    output,
                    is_error: false,
                }
            }
            Ok(Err(e)) => {
                warn!(patom.tool = %call.name, error = %e, "tool.result.err");
                error_result(id, e.to_string())
            }
            Err(_) => {
                warn!(patom.tool = %call.name, "tool.timeout");
                error_result(id, format!("tool `{}` timed out", call.name))
            }
        }
    }
}

/// Map a `StopReason` to the short label `turn_metrics.stop_reason`
/// stores. Stable strings the dashboard can group on; matches the set
/// pinned in migration 44's column comment
/// (`end_turn | tool_use | length | other:<provider-detail>`).
/// `StopReasonLabel::from_truncated` clips to the column CHECK ceiling.
fn stop_reason_label(stop: &StopReason) -> String {
    match stop {
        StopReason::EndTurn => "end_turn".to_owned(),
        StopReason::ToolUse => "tool_use".to_owned(),
        StopReason::MaxTokens => "length".to_owned(),
        StopReason::Other(detail) => format!("other:{detail}"),
    }
}

fn error_result(call_id: ToolCallId, message: String) -> ToolResult {
    // Defence in depth: cap error messages too. A misbehaving tool could
    // otherwise embed an upstream body and blow the budget.
    let mut output = message;
    if output.len() > TOOL_RESULT_MAX_BYTES {
        truncate_to_char_boundary(&mut output, TOOL_RESULT_MAX_BYTES);
    }
    ToolResult {
        call_id,
        output,
        is_error: true,
    }
}

/// One tool dispatch's bundled output: the result the model sees, plus
/// the timing the `tool_calls` recorder writes.
///
/// Threaded out of `run_call_with_hooks` so the dispatcher merge step
/// has every column it needs without re-deriving timing per call.
#[derive(Debug)]
struct CallOutcome {
    result: ToolResult,
    started_at: DateTime<Utc>,
    duration: StdDuration,
}

/// Helper to construct a `TurnIndex` from a loop counter inside the
/// bounded `0..max_turns` range.
pub(super) fn turn_index(turn: u32) -> TurnIndex {
    TurnIndex::try_from(turn).expect("invariant: max_turns is bounded so loop index fits TurnIndex")
}

/// Per-call dispatch classification. Mirrors the three resolved
/// states `run_tools` must distinguish: a known concurrency-safe tool
/// (joinable into the current batch), a known unsafe tool (barrier),
/// and an unknown name (also a barrier — `run_one_tool` will turn it
/// into an `is_error` `ToolResult`).
#[derive(Debug, Clone)]
enum CallClass {
    Safe(SharedTool),
    Unsafe(SharedTool),
    Unknown,
}

impl CallClass {
    fn classify(resolved: Option<SharedTool>) -> Self {
        match resolved {
            Some(t) if t.concurrency_safe() => Self::Safe(t),
            Some(t) => Self::Unsafe(t),
            None => Self::Unknown,
        }
    }

    fn is_safe(&self) -> bool {
        matches!(self, Self::Safe(_))
    }

    fn tool(&self) -> Option<SharedTool> {
        match self {
            Self::Safe(t) | Self::Unsafe(t) => Some(t.clone()),
            Self::Unknown => None,
        }
    }
}

/// Fuse consecutive `true` entries into a single range; each `false`
/// becomes a singleton. Preserves input order; covers `0..classes.len()`
/// exactly once.
pub(super) fn plan_batches(classes: &[bool]) -> Vec<std::ops::Range<usize>> {
    let mut out: Vec<std::ops::Range<usize>> = Vec::new();
    let mut i = 0;
    while i < classes.len() {
        let mut j = i + 1;
        if classes[i] {
            while j < classes.len() && classes[j] {
                j += 1;
            }
        }
        out.push(i..j);
        i = j;
    }
    out
}

#[cfg(test)]
mod plan_batches_tests {
    use super::plan_batches;

    #[test]
    fn empty_input_yields_no_batches() {
        let batches = plan_batches(&[]);
        assert!(batches.is_empty());
    }

    #[test]
    fn single_safe_call_is_one_singleton_batch() {
        let batches = plan_batches(&[true]);
        assert_eq!(batches, vec![0..1]);
    }

    #[test]
    fn single_unsafe_call_is_one_singleton_batch() {
        let batches = plan_batches(&[false]);
        assert_eq!(batches, vec![0..1]);
    }

    #[test]
    fn consecutive_safe_calls_fuse_into_one_batch() {
        let batches = plan_batches(&[true, true, true]);
        assert_eq!(batches, vec![0..3]);
    }

    #[test]
    fn unsafe_call_breaks_the_batch() {
        // [A_safe, B_safe, C_unsafe, D_safe, E_safe, F_safe]
        // → [{A,B}, {C}, {D,E,F}]
        let batches = plan_batches(&[true, true, false, true, true, true]);
        assert_eq!(batches, vec![0..2, 2..3, 3..6]);
    }

    #[test]
    fn alternating_unsafe_and_safe_yields_singletons_then_runs() {
        let batches = plan_batches(&[false, true, false, true, true]);
        assert_eq!(batches, vec![0..1, 1..2, 2..3, 3..5]);
    }

    #[test]
    fn every_call_is_visited_exactly_once_in_order() {
        let classes = [true, false, true, true, false, false, true];
        let batches = plan_batches(&classes);
        let mut covered: Vec<usize> = Vec::new();
        for b in batches {
            covered.extend(b);
        }
        assert_eq!(covered, (0..classes.len()).collect::<Vec<_>>());
    }
}
