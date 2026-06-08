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
use crate::hook::{ToolContext, TurnContext};
use crate::provider::{
    ChatMessage, ChatRequest, ChatResponse, StopReason, ToolCall, ToolCallId, ToolResult,
    UserContent,
};
use crate::runtime::{PromptRequestId, RequestKind, RequestKindPayload};
use crate::session::SessionId;
use crate::tools::{
    SharedTool, TOOL_RESULT_MAX_BYTES, ToolBox, ToolCallContext, ToolCallRow, ToolCallRowId,
    clip_error_message, truncate_to_char_boundary,
};
use crate::types::{MessageSender, Participant, TurnIndex};

use crate::auth::OrgId;
use crate::budget::{BudgetError, price_for, turn_cost};

use super::core::{Agent, send_message_tool_name};
use super::error::AgentError;
use super::limits::MAX_TOOL_CALLS_PER_TURN;
use super::log;
use super::observer::SharedTurnObserver;
use super::outcome::viewer_kind;
use super::turn_metrics::{
    DurationMs, InputTokens, OutputTokens, StopReasonLabel, TurnMetricsId, TurnMetricsRow,
};

impl Agent {
    /// Run one provider call + its tool-call follow-up. Returns `Some(text)` when
    /// the turn ends with a final answer; `None` to continue the loop.
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn run_turn(
        &self,
        ctx: TurnContext,
        viewer: Participant,
        counterpart: Participant,
        viewer_as_sender: MessageSender,
        root_request_id: PromptRequestId,
        request_id: PromptRequestId,
        caller: Caller,
        kind_payload: &RequestKindPayload,
        send_message_calls: &mut usize,
        cancel: &CancellationToken,
        observer: Option<&SharedTurnObserver>,
    ) -> Result<Option<String>, AgentError> {
        self.hooks().before_turn(ctx).await?.into_result()?;
        // Spend gate before the (paid) provider call. Stops a long-running DAG
        // the moment it crosses the org's monthly cap; the HTTP admission gate
        // only checked the root prompt.
        self.budget_gate(caller.org_id).await?;
        // Wall-clock `started_at` from the agent clock (CLAUDE.md §11) so
        // tests can pin timestamps; `started_mono` runs alongside so a
        // paused / faked wall clock cannot zero out `duration_ms`.
        let started_at = self.clock().now_utc();
        let started_mono = Instant::now();
        let response = self
            .send_one_turn(ctx.session_id, viewer, counterpart, kind_payload, cancel)
            .await?;
        let duration = started_mono.elapsed();
        self.record_turn_metrics(
            request_id,
            ctx.session_id,
            caller.org_id,
            kind_payload.kind(),
            started_at,
            duration,
            &response,
        )
        .await;
        // Post-paid settle: charge the org for what this turn actually cost.
        // Awaited but fail-open (see `budget_settle`).
        self.budget_settle(caller.org_id, &response).await;
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

        self.sessions()
            .append_for_user(
                caller.user_id,
                ctx.session_id,
                viewer_as_sender,
                counterpart,
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

        let tool_ctx = ToolCallContext {
            session_id: ctx.session_id,
            viewer,
            root_request_id,
            request_id,
            kind_payload: kind_payload.clone(),
            acting_user_id: caller.user_id,
            org_id: caller.org_id,
        };
        // Counted regardless of tool error — the model already saw the failure
        // via the tool result; the worker's ping-pong guard cares only about
        // attempts to deliver.
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
        // Sender = `System` so the row renders to viewer-as-User without
        // claiming the human authored the result.
        self.sessions()
            .append_for_user(
                caller.user_id,
                ctx.session_id,
                MessageSender::System,
                viewer,
                ChatMessage::User(results.into_iter().map(UserContent::ToolResult).collect()),
                request_id,
            )
            .await?;
        Ok(None)
    }

    /// Pre-turn spend gate. Returns [`AgentError::BudgetExceeded`] when the org
    /// is at/over its monthly cap. A DB error fails *open* — a transient blip
    /// must not block a turn the admission gate already admitted; the counter is
    /// reconciled from `turn_metrics`. No-op when no budget service is wired
    /// (agent_core unit tests).
    async fn budget_gate(&self, org: OrgId) -> Result<(), AgentError> {
        let Some(budget) = self.budget() else {
            return Ok(());
        };
        match budget.check_or_fail(org).await {
            Ok(()) => Ok(()),
            Err(BudgetError::Exceeded { .. }) => Err(AgentError::BudgetExceeded { org }),
            Err(BudgetError::Db(e)) => {
                tracing::error!(
                    error = ?e,
                    patom.org.id = %org,
                    "budget.gate.db_error_fail_open",
                );
                Ok(())
            }
        }
    }

    /// Post-paid settle: add this turn's actual cost to the org's current
    /// period. Fail-open — a settle failure is logged and the turn proceeds (the
    /// user already received the answer); `turn_metrics` is the reconciliation
    /// ledger (CLAUDE.md §6). No-op when no budget service is wired.
    async fn budget_settle(&self, org: OrgId, response: &ChatResponse) {
        let Some(budget) = self.budget() else {
            return;
        };
        let cost = turn_cost(price_for(self.model()), &response.usage);
        if let Err(e) = budget.settle(org, cost).await {
            tracing::error!(
                error = ?e,
                patom.org.id = %org,
                "budget.settle.failed",
            );
        }
    }

    /// Best-effort write to `turn_metrics`. Skipped when the recorder is
    /// not wired (agent_core unit tests). DB / conversion failures emit
    /// `tracing::error!` and continue — the user has already seen the
    /// turn (CLAUDE.md §6: observability never blocks the user-visible
    /// path; the row going missing is one chart cell, not a turn replay).
    #[allow(clippy::too_many_arguments)] // recorder bundles per-call audit fields, not branching
    async fn record_turn_metrics(
        &self,
        request_id: PromptRequestId,
        session_id: SessionId,
        org_id: crate::auth::OrgId,
        kind: RequestKind,
        started_at: chrono::DateTime<chrono::Utc>,
        duration: StdDuration,
        response: &ChatResponse,
    ) {
        let Some(binding) = self.turn_metrics() else {
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
            session_id,
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

    pub(super) async fn send_one_turn(
        &self,
        session: SessionId,
        viewer: Participant,
        counterpart: Participant,
        kind_payload: &RequestKindPayload,
        cancel: &CancellationToken,
    ) -> Result<ChatResponse, AgentError> {
        let request = self
            .build_chat_request(session, viewer, counterpart, kind_payload)
            .await?;
        self.call_provider(request, self.provider_timeout(), cancel)
            .await
    }

    /// Single LLM provider entry point. Every code path that talks to a
    /// model — normal turn, reflection, resolution — funnels through here
    /// so timeout, cancellation, and error mapping live in one place.
    pub(super) async fn call_provider(
        &self,
        request: ChatRequest,
        timeout_after: std::time::Duration,
        cancel: &CancellationToken,
    ) -> Result<ChatResponse, AgentError> {
        let send = self.provider().send(request);
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

    /// Assemble the per-turn provider request: own-session history, optional
    /// parent-session prefix, system prompt, tool specs.
    #[tracing::instrument(
        skip_all,
        name = "session.context.build",
        fields(
            patom.session.id = %session,
            patom.viewer = %viewer,
            patom.viewer.kind = viewer_kind(viewer),
            patom.history.count = tracing::field::Empty,
            patom.parent_session.included = tracing::field::Empty,
            patom.parent_session.history.count = tracing::field::Empty,
            patom.system_prompt.bytes = tracing::field::Empty,
            patom.messages.count = tracing::field::Empty,
        ),
    )]
    async fn build_chat_request(
        &self,
        session: SessionId,
        viewer: Participant,
        counterpart: Participant,
        kind_payload: &RequestKindPayload,
    ) -> Result<ChatRequest, AgentError> {
        let kind = kind_payload.kind();
        let span = tracing::Span::current();
        // §6: worker turns only ever run for an agent viewer (real colleague).
        // Surface a backend-error rather than panic if a caller breaks the
        // invariant — the trait signature speaks `ColleagueId` honestly.
        let viewer_colleague = viewer.colleague_id().ok_or_else(|| {
            crate::session::SessionError::Backend(
                "build_chat_request called with System viewer; worker invariant".to_string(),
            )
        })?;
        let own = self.sessions().snapshot(session, viewer_colleague).await?;
        assert!(
            !own.is_empty(),
            "session must contain at least the user prompt"
        );
        span.record("patom.history.count", own.len());

        // Prepend the immediate parent session's history when the viewer
        // participates in the parent — i.e. the agent's own conversation
        // continues across the fork (e.g. `default` reading `human↔default`
        // while processing a reply from `default↔translator`). Foreign viewers
        // get an empty parent history; framing comes through `send_message`'s
        // `context_summary`, with `get_session` for deeper lookups.
        let parent = self
            .sessions()
            .parent_history_for_viewer(session, viewer_colleague)
            .await?;
        span.record("patom.parent_session.included", !parent.is_empty());
        span.record("patom.parent_session.history.count", parent.len());

        let mut messages: Vec<ChatMessage> = Vec::with_capacity(parent.len() + own.len());
        messages.extend(parent);
        messages.extend(own);

        let memory_system = self
            .memory()
            .system_prompt(session, viewer, counterpart, kind_payload)
            .await?;
        // Fold the session's current todo list into the system prompt
        // tail. Empty / missing-store cases render to the empty string,
        // which `format!` below leaves as a no-op (no trailing
        // separator). See `tools::system::todos::render_section` for
        // the block format.
        let todos_block = match self.todos_store() {
            Some(store) => {
                // CLAUDE.md §5: every I/O await is wrapped. PK-lookup
                // against a single row; the bound here just keeps a
                // stalled pool/connection from holding the turn hostage.
                let list =
                    tokio::time::timeout(super::limits::TODOS_LOAD_TIMEOUT, store.get(session))
                        .await
                        .map_err(|_| AgentError::TodosLoadTimeout)??;
                crate::tools::system::todos::render_section(&list)
            }
            None => String::new(),
        };
        let system: std::sync::Arc<str> = if todos_block.is_empty() {
            memory_system
        } else {
            std::sync::Arc::from(format!("{memory_system}\n{todos_block}").as_str())
        };
        span.record("patom.system_prompt.bytes", system.len());
        span.record("patom.messages.count", messages.len());

        let tools = self.tools().specs_for(kind);
        Ok(ChatRequest {
            model: self.model(),
            system,
            messages,
            tools,
            max_output_tokens: self.max_output_tokens(),
        })
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
    /// store is wired or the viewer is not an agent (system / human
    /// participants never dispatch tools but the type system can't
    /// prove it here). DB failures emit `tracing::error!` and continue
    /// — the user has already seen the result.
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
        // Carries *what* failed for the audit row, not just *that* it failed.
        let error_message = outcome
            .result
            .is_error
            .then(|| clip_error_message(outcome.result.output.clone()));
        let row = ToolCallRow {
            id: ToolCallRowId::new(),
            org_id: tool_ctx.org_id,
            session_id: tool_ctx.session_id,
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
            session_id: ctx.session_id,
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
            patom.session.id = %tool_ctx.session_id.as_uuid(),
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
