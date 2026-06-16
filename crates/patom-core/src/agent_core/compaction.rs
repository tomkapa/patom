//! Context compaction — bound the per-turn thread prompt (#182).
//!
//! Two layers (see `doc/context-compaction-plan.md`):
//!  - a **windowing floor** enforced in `threads::pg_store` (a hard `LIMIT`),
//!    which bounds the prompt on every turn with no LLM in the loop; and
//!  - a best-effort **rolling per-(thread, agent) summary** folded in here when
//!    the verbatim tail would overflow the model's context window.
//!
//! This module owns the LLM-facing half (the storage primitives live next to
//! the feed). Stage 1 lands only the cheap, dependency-free token estimator the
//! trigger consults; the `Compactor` and `resolve_agent_context` orchestration
//! arrive in later stages.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};

use crate::agents::AgentId;
use crate::auth::OrgId;
use crate::clock::SharedClock;
use crate::colleagues::{ColleagueId, ColleagueName};
use crate::provider::{
    AssistantContent, ChatMessage, ChatRequest, ChatResponse, Model, ProviderError, SharedProvider,
    ToolCallId, UserContent,
};
use crate::runtime::{MetricKind, PromptRequestId};
use crate::threads::{AgentThreadId, ContextTail, MAX_CONTEXT_MESSAGES, Seq, TailRow, ThreadId};
use crate::types::MaxOutputTokens;

use super::core::Agent;
use super::error::AgentError;
use super::limits::{
    COMPACTION_COOLDOWN, COMPACTION_LLM_TIMEOUT, CONTEXT_TOKEN_BUDGET_DIVISOR,
    MAX_COMPACTION_CHUNKS, MAX_COMPACTION_WALL_CLOCK, MAX_SUMMARY_TOKENS, SUMMARIZER_INPUT_BUDGET,
};

/// Approximate token cost of a chunk of prompt text.
///
/// A newtype (CLAUDE.md §1) so an estimate can't be silently compared against a
/// real `input_tokens` count or a byte length. The estimate is deliberately
/// crude — `chars / 4`, the standard rule of thumb — and used only to *trigger*
/// compaction and pick a cut-point, never to bill. Real `input_tokens` from
/// `turn_metrics` calibrates it over time (no tokenizer crate, CLAUDE.md §8).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct TokenEstimate(u32);

impl TokenEstimate {
    /// Estimated token count.
    #[must_use]
    pub const fn get(self) -> u32 {
        self.0
    }

    /// Saturating `u32` construction from a `usize` char count, divided by the
    /// `chars / 4` heuristic. Saturates rather than narrowing with `as`
    /// (CLAUDE.md §7) so a pathological length can't wrap.
    fn from_chars(chars: usize) -> Self {
        let tokens = chars / CHARS_PER_TOKEN;
        Self(u32::try_from(tokens).unwrap_or(u32::MAX))
    }
}

/// The `chars`-per-token divisor for the estimate. Four is the conventional
/// English approximation; calibrated against recorded `input_tokens`.
const CHARS_PER_TOKEN: usize = 4;

/// Estimate the prompt-token cost of an optional rolling summary plus the tail.
///
/// Counts every text-bearing field (text, reasoning, tool-call input JSON,
/// tool-result output); structural overhead is ignored — the estimate is a
/// trigger signal, not an accounting figure.
#[must_use]
pub fn estimate_tokens<'a>(
    summary: Option<&str>,
    tail: impl IntoIterator<Item = &'a ChatMessage>,
) -> TokenEstimate {
    let mut chars: usize = summary.map_or(0, str::len);
    for message in tail {
        chars = chars.saturating_add(message_chars(message));
    }
    TokenEstimate::from_chars(chars)
}

/// Sum the char length of every text-bearing field in one message.
fn message_chars(message: &ChatMessage) -> usize {
    let mut chars: usize = 0;
    match message {
        ChatMessage::User(contents) => {
            for content in contents {
                let len = match content {
                    UserContent::Text(text) => text.len(),
                    UserContent::ToolResult(result) => result.output.len(),
                };
                chars = chars.saturating_add(len);
            }
        }
        ChatMessage::Assistant(contents) => {
            for content in contents {
                let len = match content {
                    AssistantContent::Text(text) | AssistantContent::Reasoning(text) => text.len(),
                    // Tool-call inputs are arbitrary JSON; its serialized form
                    // is what the provider bills, so estimate against that.
                    AssistantContent::ToolCall(call) => call.input.to_string().len(),
                };
                chars = chars.saturating_add(len);
            }
        }
    }
    chars
}

/// Split a verbatim tail into `(overflow, keep)` at a tool-pair-safe boundary.
///
/// The boundary **never** separates a `tool_use` (an assistant `ToolCall`) from
/// its matching `tool_result` (a user `ToolResult`). Providers reject an orphaned
/// pair, so a naive "keep the last N" cut would corrupt the request.
///
/// `keep` is the most-recent slice — at least `target_keep` messages, possibly
/// one more when the natural boundary would land between a pair (the boundary is
/// nudged earlier to pull the whole pair into `keep`). `overflow` is everything
/// older, handed to the summarizer. After `repair_tool_pairs` a `tool_result`
/// row immediately follows its `tool_use`, so one boundary adjustment suffices.
///
/// Returns `(overflow, keep)`. When `target_keep >= tail.len()` the overflow is
/// empty (nothing to compact).
#[must_use]
pub fn cut_at_tool_safe_boundary(
    mut tail: Vec<ChatMessage>,
    target_keep: usize,
) -> (Vec<ChatMessage>, Vec<ChatMessage>) {
    let cut = tool_safe_cut_index(&tail, target_keep);
    let keep = tail.split_off(cut);
    (tail, keep)
}

/// The split index `k`: `tail[..k]` is overflow, `tail[k..]` is the kept tail.
///
/// Chosen so the boundary never falls between an adjacent `tool_use`/`tool_result`
/// pair. Callers that carry a parallel `seq` vec split it at the same index to
/// learn how far the overflow reached. See [`cut_at_tool_safe_boundary`].
#[must_use]
pub fn tool_safe_cut_index(tail: &[ChatMessage], target_keep: usize) -> usize {
    let len = tail.len();
    let mut cut = len.saturating_sub(target_keep);
    // If the boundary would orphan a `tool_result` at the head of `keep`, pull
    // its `tool_use` (the immediately-preceding assistant row) into `keep` too.
    if cut > 0
        && cut < len
        && let Some(call_id) = tool_result_head(&tail[cut])
        && assistant_calls(&tail[cut - 1], call_id)
    {
        cut -= 1;
    }
    cut
}

/// If `message` is a user row whose leading content is a `tool_result`, return
/// the call id it answers.
fn tool_result_head(message: &ChatMessage) -> Option<&ToolCallId> {
    match message {
        ChatMessage::User(contents) => match contents.first() {
            Some(UserContent::ToolResult(result)) => Some(&result.call_id),
            _ => None,
        },
        ChatMessage::Assistant(_) => None,
    }
}

/// Whether `message` is an assistant row issuing the `tool_use` with `call_id`.
fn assistant_calls(message: &ChatMessage, call_id: &ToolCallId) -> bool {
    match message {
        ChatMessage::Assistant(contents) => contents.iter().any(
            |content| matches!(content, AssistantContent::ToolCall(call) if &call.id == call_id),
        ),
        ChatMessage::User(_) => false,
    }
}

/// The structured rolling summary of an agent's earlier context in a thread.
///
/// A newtype (CLAUDE.md §1) capped at `MAX_SUMMARY_TOKENS` so it can't grow
/// without bound across folds. `clamp` is the always-succeeding constructor the
/// summarizer uses (truncates an over-long model output rather than discarding a
/// whole compaction); `TryFrom` is the strict boundary parser.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompactionSummary(String);

impl CompactionSummary {
    /// The summary text, for rendering into the system prefix.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Estimated token cost (`chars / 4`), for budgeting and storage.
    #[must_use]
    pub fn estimated_tokens(&self) -> u32 {
        u32::try_from(self.0.chars().count() / CHARS_PER_TOKEN).unwrap_or(u32::MAX)
    }

    /// Wrap text, truncating (char-safe) to `MAX_SUMMARY_TOKENS`. Used by the
    /// summarizer: a slightly-too-long model output is trimmed, never rejected.
    #[must_use]
    pub fn clamp(text: String) -> Self {
        let cap_chars = usize::try_from(MAX_SUMMARY_TOKENS)
            .unwrap_or(usize::MAX)
            .saturating_mul(CHARS_PER_TOKEN);
        if text.chars().count() <= cap_chars {
            return Self(text);
        }
        Self(text.chars().take(cap_chars).collect())
    }
}

impl TryFrom<String> for CompactionSummary {
    type Error = crate::types::ParseError;
    fn try_from(text: String) -> Result<Self, Self::Error> {
        if text.trim().is_empty() {
            return Err(crate::types::ParseError::Empty {
                field: "compaction_summary",
            });
        }
        let tokens = text.chars().count() / CHARS_PER_TOKEN;
        let cap = usize::try_from(MAX_SUMMARY_TOKENS).unwrap_or(usize::MAX);
        if tokens > cap {
            return Err(crate::types::ParseError::TooLong {
                field: "compaction_summary",
                max: cap,
                got: tokens,
            });
        }
        Ok(Self(text))
    }
}

/// One module-boundary error type for the summarizer (CLAUDE.md §12).
///
/// Every variant is *recoverable* at the call site: a turn that hits one falls
/// back to the windowing floor, so this never propagates as a turn failure.
#[derive(Debug, thiserror::Error)]
pub enum CompactionError {
    #[error("summarizer call timed out")]
    SummarizerTimeout,
    #[error("summarizer provider error: {0}")]
    Provider(#[from] ProviderError),
    #[error("summarizer returned no usable content")]
    Empty,
    #[error("compaction exceeded its wall-clock budget")]
    WallClockExceeded,
}

/// The assembled per-turn context: an optional rolling summary (folded into the
/// system prefix) plus the bounded verbatim message tail.
#[derive(Debug, Clone, Default)]
pub struct AgentContext {
    pub summary: Option<CompactionSummary>,
    pub messages: Vec<ChatMessage>,
}

/// One summarizer fold's metering payload, drained by the caller into a
/// `turn_metrics` row so compaction spend is billed to the org.
#[derive(Debug)]
pub(super) struct FoldSample {
    pub started_at: DateTime<Utc>,
    pub duration: Duration,
    pub response: ChatResponse,
}

/// The turn's resolved routing + identity, threaded into context assembly so a
/// compaction fold uses the same provider/BYO decision as the turn and meters
/// under the turn's `request_id`. Bundled to keep `build_thread_request`'s arity
/// sane (CLAUDE.md §4).
pub(super) struct TurnRouting<'a> {
    pub provider: &'a SharedProvider,
    pub request_id: PromptRequestId,
    pub org: OrgId,
}

/// Per-turn prompt token budget — a fraction of the model's window.
fn context_token_budget(model: Model) -> u32 {
    model.context_window().get() / CONTEXT_TOKEN_BUDGET_DIVISOR
}

/// Number of most-recent rows whose cumulative estimate stays within
/// `keep_budget` tokens — at least one, never more than the tail length.
fn keep_count(rows: &[TailRow], keep_budget: u32) -> usize {
    let mut acc: u32 = 0;
    let mut count: usize = 0;
    for row in rows.iter().rev() {
        let tokens =
            u32::try_from(message_chars(&row.message) / CHARS_PER_TOKEN).unwrap_or(u32::MAX);
        acc = acc.saturating_add(tokens);
        if acc > keep_budget && count >= 1 {
            break;
        }
        count += 1;
    }
    count.clamp(1, rows.len().max(1))
}

/// Split `overflow` into chunks each under `budget` estimated tokens, oldest
/// first. A single over-budget message becomes its own chunk (never dropped here).
fn split_chunks(overflow: Vec<ChatMessage>, budget: u32) -> Vec<Vec<ChatMessage>> {
    let mut chunks: Vec<Vec<ChatMessage>> = Vec::new();
    let mut current: Vec<ChatMessage> = Vec::new();
    let mut acc: u32 = 0;
    for message in overflow {
        let tokens = u32::try_from(message_chars(&message) / CHARS_PER_TOKEN).unwrap_or(u32::MAX);
        if acc.saturating_add(tokens) > budget && !current.is_empty() {
            chunks.push(std::mem::take(&mut current));
            acc = 0;
        }
        acc = acc.saturating_add(tokens);
        current.push(message);
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    chunks
}

/// The empty rolling-summary template — the section skeleton the first fold fills.
fn empty_summary_template() -> String {
    "Facts:\nDecisions:\nConstraints:\nOpen items:\nProgress:".to_string()
}

/// System prompt for a summarizer fold (the Hermes "update, don't regenerate" rule).
const FOLD_SYSTEM_PROMPT: &str = "You maintain a rolling summary of an ongoing \
    multi-party conversation so older turns can be dropped from an agent's context \
    without losing meaning. You are given the PREVIOUS SUMMARY and NEW MESSAGES. \
    Update the summary to fold in the new messages, preserving earlier facts unless \
    they are contradicted. Keep exactly these sections, each a terse bullet list: \
    Facts, Decisions, Constraints, Open items, Progress. Output ONLY the updated \
    summary text — no preamble, no commentary.";

/// Render a chunk of messages into plain text for the summarizer to read.
fn render_messages_for_summary(messages: &[ChatMessage]) -> String {
    let mut out = String::new();
    for message in messages {
        match message {
            ChatMessage::User(contents) => {
                for content in contents {
                    match content {
                        UserContent::Text(text) => {
                            out.push_str("User: ");
                            out.push_str(text);
                            out.push('\n');
                        }
                        UserContent::ToolResult(result) => {
                            out.push_str("Tool result: ");
                            out.push_str(&result.output);
                            out.push('\n');
                        }
                    }
                }
            }
            ChatMessage::Assistant(contents) => {
                for content in contents {
                    match content {
                        AssistantContent::Text(text) => {
                            out.push_str("Assistant: ");
                            out.push_str(text);
                            out.push('\n');
                        }
                        AssistantContent::Reasoning(text) => {
                            out.push_str("Assistant (reasoning): ");
                            out.push_str(text);
                            out.push('\n');
                        }
                        AssistantContent::ToolCall(call) => {
                            out.push_str("Assistant called tool ");
                            out.push_str(call.name.as_str());
                            out.push('\n');
                        }
                    }
                }
            }
        }
    }
    out
}

/// Concatenate the assistant text blocks of a summarizer response, or `None` if
/// the model returned nothing usable.
fn extract_text(response: &ChatResponse) -> Option<String> {
    let mut out = String::new();
    for content in &response.content {
        if let AssistantContent::Text(text) = content {
            out.push_str(text);
        }
    }
    if out.trim().is_empty() {
        None
    } else {
        Some(out)
    }
}

/// The summarizer: chunked, bounded, rolling-fold compactor (CLAUDE.md §4/§5).
/// Holds no provider of its own — it is handed the turn's resolved provider per
/// call (the "reuse the agent's own model" decision).
#[derive(Debug)]
pub(super) struct Compactor {
    clock: SharedClock,
    model: Model,
    max_output_tokens: MaxOutputTokens,
}

impl Compactor {
    pub(super) fn new(
        clock: SharedClock,
        model: Model,
        max_output_tokens: MaxOutputTokens,
    ) -> Self {
        Self {
            clock,
            model,
            max_output_tokens,
        }
    }

    /// Fold `overflow` into a rolling summary. Each fold is one `provider.send`
    /// under `COMPACTION_LLM_TIMEOUT`; the whole pass is bounded by `deadline`
    /// (total wall-clock) and `MAX_COMPACTION_CHUNKS` (loop bound). Pushes a
    /// `FoldSample` per successful fold so the caller can meter it, even if a
    /// later fold then fails.
    pub(super) async fn summarize(
        &self,
        provider: &SharedProvider,
        prev: Option<&str>,
        overflow: Vec<ChatMessage>,
        deadline: Instant,
        samples: &mut Vec<FoldSample>,
    ) -> Result<CompactionSummary, CompactionError> {
        let mut chunks = split_chunks(overflow, SUMMARIZER_INPUT_BUDGET);
        if chunks.len() > MAX_COMPACTION_CHUNKS {
            let dropped = chunks.len() - MAX_COMPACTION_CHUNKS;
            // No silent cap (CLAUDE.md §5): drop the oldest beyond the bound and
            // record it. Only reachable on the first compaction of a huge thread.
            tracing::warn!(
                event = "patom.compaction.chunks_dropped",
                patom.compaction.chunks_dropped = dropped,
                "compaction overflow exceeded MAX_COMPACTION_CHUNKS; dropping oldest chunks",
            );
            chunks.drain(0..dropped);
        }
        assert!(
            chunks.len() <= MAX_COMPACTION_CHUNKS,
            "fold loop is bounded"
        );

        let mut acc = prev.map_or_else(empty_summary_template, str::to_owned);
        for chunk in chunks {
            if self.clock.now() >= deadline {
                return Err(CompactionError::WallClockExceeded);
            }
            let started_at = self.clock.now_utc();
            let started = self.clock.now();
            let request = self.fold_request(&acc, &chunk);
            let response = tokio::time::timeout(COMPACTION_LLM_TIMEOUT, provider.send(request))
                .await
                .map_err(|_| CompactionError::SummarizerTimeout)?
                .map_err(CompactionError::Provider)?;
            let duration = self.clock.now().saturating_duration_since(started);
            samples.push(FoldSample {
                started_at,
                duration,
                response: response.clone(),
            });
            acc = extract_text(&response).ok_or(CompactionError::Empty)?;
        }
        Ok(CompactionSummary::clamp(acc))
    }

    /// Build one fold request: the previous summary + the new messages, under the
    /// agent's own model. No tools (summarization is a pure text turn).
    fn fold_request(&self, acc: &str, chunk: &[ChatMessage]) -> ChatRequest {
        let user = format!(
            "PREVIOUS SUMMARY:\n{acc}\n\nNEW MESSAGES TO FOLD IN (oldest first):\n{}",
            render_messages_for_summary(chunk)
        );
        ChatRequest {
            model: self.model,
            system: Arc::from(FOLD_SYSTEM_PROMPT),
            messages: vec![ChatMessage::User(vec![UserContent::Text(user)])],
            tools: Arc::from([]),
            max_output_tokens: self.max_output_tokens,
        }
    }
}

impl Agent {
    /// Assemble `agent`'s bounded context for a turn: the rolling summary (system
    /// prefix) + the verbatim tail (messages). The windowing floor in
    /// `context_tail` bounds the prompt unconditionally; this adds the best-effort
    /// summary when the tail would overflow the model's token budget.
    pub(super) async fn resolve_agent_context(
        &self,
        thread: ThreadId,
        agent: AgentId,
        viewer: ColleagueId,
        overrides: &HashMap<ColleagueId, ColleagueName>,
        state_id: AgentThreadId,
        routing: &TurnRouting<'_>,
    ) -> Result<AgentContext, AgentError> {
        let comp = self.threads().load_compaction(thread, agent).await?;
        // Destructure once: *move* the summary (this runs every turn, so a clone
        // here is pure waste on the common no-overflow path) and copy the cheap
        // Copy fields.
        let (prev, since, cooldown_until) = match comp {
            Some(c) => (
                Some(c.summary).filter(|s| !s.is_empty()),
                c.covers_through_seq,
                c.cooldown_until,
            ),
            None => (None, Seq::ZERO, None),
        };
        let tail = self
            .threads()
            .context_tail(thread, agent, viewer, since, overrides)
            .await?;

        let budget = context_token_budget(self.model());
        let est = estimate_tokens(prev.as_deref(), tail.rows.iter().map(|r| &r.message));
        let max_msgs = usize::try_from(MAX_CONTEXT_MESSAGES).unwrap_or(usize::MAX);
        if est.get() <= budget && tail.len() <= max_msgs {
            return Ok(floor_context(prev, tail)); // COMMON PATH — no LLM
        }

        // A recent summarizer failure: skip the LLM, serve the floor + stale summary.
        let now = self.clock().now_utc();
        if cooldown_until.is_some_and(|until| now < until) {
            tracing::info!(
                event = "patom.compaction.cooldown_skipped",
                patom.agent.id = %agent,
                patom.thread.id = %thread,
            );
            return Ok(floor_context(prev, tail));
        }

        self.compact_overflow(thread, agent, prev, tail, state_id, routing)
            .await
    }

    /// The overflow path: cut a tool-safe boundary, summarize the older slice,
    /// persist it, and return summary + the kept tail. On any summarizer failure
    /// the prompt still holds — fall back to the floor and start a cooldown.
    async fn compact_overflow(
        &self,
        thread: ThreadId,
        agent: AgentId,
        prev: Option<String>,
        tail: ContextTail,
        state_id: AgentThreadId,
        routing: &TurnRouting<'_>,
    ) -> Result<AgentContext, AgentError> {
        let keep_budget = context_token_budget(self.model()) / 2;
        let target_keep = keep_count(&tail.rows, keep_budget);
        // Seqs are `Copy` (collect cheaply); the messages are *moved* out of the
        // rows, not cloned, before we split them into overflow + keep.
        let seqs: Vec<Seq> = tail.rows.iter().map(|r| r.seq).collect();
        let mut messages: Vec<ChatMessage> = tail.rows.into_iter().map(|r| r.message).collect();
        let cut = tool_safe_cut_index(&messages, target_keep);
        if cut == 0 {
            // The tool-safe nudge left nothing to summarize; serve the floor.
            return Ok(AgentContext {
                summary: prev.map(CompactionSummary::clamp),
                messages,
            });
        }
        let covers = seqs[..cut].iter().copied().max();
        let keep = messages.split_off(cut);
        let overflow = messages;

        let deadline = self.clock().now() + MAX_COMPACTION_WALL_CLOCK;
        let compactor =
            Compactor::new(self.clock().clone(), self.model(), self.max_output_tokens());
        let mut samples: Vec<FoldSample> = Vec::new();
        let result = compactor
            .summarize(
                routing.provider,
                prev.as_deref(),
                overflow,
                deadline,
                &mut samples,
            )
            .await;

        // Meter every fold we actually ran — even on eventual failure — so org
        // billing is accurate (the user's "meter to org" decision).
        for sample in &samples {
            self.record_turn_metrics(
                routing.request_id,
                Some(state_id),
                routing.org,
                MetricKind::Compaction,
                sample.started_at,
                sample.duration,
                &sample.response,
            )
            .await;
        }

        match result {
            Ok(summary) => {
                tracing::info!(
                    event = "patom.compaction.triggered",
                    patom.agent.id = %agent,
                    patom.thread.id = %thread,
                );
                if let Some(covers_through_seq) = covers {
                    let tokens = i32::try_from(summary.estimated_tokens()).unwrap_or(i32::MAX);
                    self.threads()
                        .save_compaction(
                            routing.org,
                            thread,
                            agent,
                            summary.as_str(),
                            covers_through_seq,
                            tokens,
                        )
                        .await?;
                }
                Ok(AgentContext {
                    summary: Some(summary),
                    messages: keep,
                })
            }
            Err(e) => {
                tracing::warn!(
                    event = "patom.compaction.failed",
                    patom.agent.id = %agent,
                    patom.thread.id = %thread,
                    error = %e,
                    "compaction summarizer failed; falling back to the windowing floor",
                );
                let until = self.clock().now_utc()
                    + chrono::Duration::from_std(COMPACTION_COOLDOWN)
                        .expect("invariant: COMPACTION_COOLDOWN is a small const within range");
                self.threads()
                    .bump_cooldown(routing.org, thread, agent, until)
                    .await?;
                Ok(AgentContext {
                    summary: prev.map(CompactionSummary::clamp),
                    messages: keep,
                })
            }
        }
    }
}

/// The no-LLM result: keep the whole windowed tail, carry the existing summary.
fn floor_context(prev: Option<String>, tail: ContextTail) -> AgentContext {
    AgentContext {
        summary: prev.map(CompactionSummary::clamp),
        messages: tail.into_messages(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_input_is_zero() {
        assert_eq!(estimate_tokens(None, &[]).get(), 0);
    }

    #[test]
    fn estimates_chars_over_four() {
        // 40 chars of user text -> 10 tokens.
        let tail = vec![ChatMessage::User(vec![UserContent::Text("x".repeat(40))])];
        assert_eq!(estimate_tokens(None, &tail).get(), 10);
    }

    #[test]
    fn summary_and_tail_are_summed() {
        let summary = "a".repeat(20); // 5 tokens
        let tail = vec![ChatMessage::Assistant(vec![AssistantContent::Text(
            "b".repeat(40), // 10 tokens
        )])];
        assert_eq!(estimate_tokens(Some(&summary), &tail).get(), 15);
    }

    #[test]
    fn counts_reasoning_and_tool_result_bodies() {
        let tail = vec![
            ChatMessage::Assistant(vec![AssistantContent::Reasoning("r".repeat(16))]), // 4
            ChatMessage::User(vec![UserContent::Text("u".repeat(8))]),                 // 2
        ];
        assert_eq!(estimate_tokens(None, &tail).get(), 6);
    }

    use crate::provider::{ToolCall, ToolResult};

    fn text_user(s: &str) -> ChatMessage {
        ChatMessage::User(vec![UserContent::Text(s.into())])
    }

    fn tool_use(id: &str) -> ChatMessage {
        ChatMessage::Assistant(vec![AssistantContent::ToolCall(ToolCall {
            id: ToolCallId::try_from(id).expect("id"),
            name: crate::types::ToolName::try_from("search").expect("name"),
            input: serde_json::json!({}),
        })])
    }

    fn tool_result(id: &str) -> ChatMessage {
        ChatMessage::User(vec![UserContent::ToolResult(ToolResult {
            call_id: ToolCallId::try_from(id).expect("id"),
            output: "ok".into(),
            is_error: false,
        })])
    }

    #[test]
    fn cut_keeps_target_when_boundary_is_clean() {
        // [u, use, result, text, u] keep 2 -> boundary at idx 3 (an assistant
        // text row), no pair split. overflow = first 3, keep = last 2.
        let tail = vec![
            text_user("a"),
            tool_use("c1"),
            tool_result("c1"),
            ChatMessage::Assistant(vec![AssistantContent::Text("t".into())]),
            text_user("b"),
        ];
        let (overflow, keep) = cut_at_tool_safe_boundary(tail, 2);
        assert_eq!(overflow.len(), 3);
        assert_eq!(keep.len(), 2);
    }

    #[test]
    fn cut_never_splits_a_pair() {
        // [u, use, result] keep 1 -> natural boundary idx 2 is the tool_result
        // head; nudged to idx 1 so the pair stays whole in keep.
        let tail = vec![text_user("a"), tool_use("c1"), tool_result("c1")];
        let (overflow, keep) = cut_at_tool_safe_boundary(tail, 1);
        assert_eq!(overflow.len(), 1, "only the lone user msg overflows");
        assert_eq!(keep.len(), 2, "tool_use + tool_result kept together");
        assert!(matches!(keep[0], ChatMessage::Assistant(_)));
    }

    #[test]
    fn cut_pulls_whole_pair_into_keep_at_the_edge() {
        // [use, result] keep 1 -> boundary nudges to 0; keep = both, overflow empty.
        let tail = vec![tool_use("c1"), tool_result("c1")];
        let (overflow, keep) = cut_at_tool_safe_boundary(tail, 1);
        assert!(overflow.is_empty());
        assert_eq!(keep.len(), 2);
    }

    #[test]
    fn cut_with_target_ge_len_overflows_nothing() {
        let tail = vec![text_user("a"), text_user("b")];
        let (overflow, keep) = cut_at_tool_safe_boundary(tail, 5);
        assert!(overflow.is_empty());
        assert_eq!(keep.len(), 2);
    }

    #[test]
    fn unrelated_tool_result_id_is_not_treated_as_a_pair() {
        // The result at the boundary answers a *different* call than the
        // preceding tool_use, so no nudge — they aren't a pair.
        let tail = vec![tool_use("c1"), tool_result("c2")];
        let (overflow, keep) = cut_at_tool_safe_boundary(tail, 1);
        assert_eq!(overflow.len(), 1);
        assert_eq!(keep.len(), 1);
    }

    // --- CompactionSummary --------------------------------------------------

    fn summary_token_cap() -> usize {
        usize::try_from(MAX_SUMMARY_TOKENS).expect("cap fits usize")
    }

    fn summarizer_input_budget() -> usize {
        usize::try_from(SUMMARIZER_INPUT_BUDGET).expect("budget fits usize")
    }

    #[test]
    fn summary_clamp_truncates_to_cap() {
        let cap_chars = summary_token_cap() * CHARS_PER_TOKEN;
        let s = CompactionSummary::clamp("z".repeat(cap_chars + 5_000));
        assert!(s.estimated_tokens() <= MAX_SUMMARY_TOKENS);
    }

    #[test]
    fn summary_try_from_rejects_empty_and_oversize() {
        assert!(CompactionSummary::try_from("   ".to_string()).is_err());
        let huge = "z".repeat((summary_token_cap() + 10) * CHARS_PER_TOKEN);
        assert!(CompactionSummary::try_from(huge).is_err());
        assert!(CompactionSummary::try_from("real summary".to_string()).is_ok());
    }

    // --- Compactor (fake provider + TestClock, no DB) -----------------------

    // `SharedClock`, `ChatResponse`, `SharedProvider`, `MaxOutputTokens`, `Arc`
    // and `ChatRequest` are already in scope via `use super::*`.
    use crate::clock::TestClock;
    use crate::provider::StopReason;
    use std::sync::Mutex as StdMutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Configurable fake summarizer: replays text, can fail, can delay (tokio),
    /// and can advance a `TestClock` per call. Records every request it sees.
    #[derive(Debug)]
    struct FakeSummarizer {
        reply: String,
        seen: StdMutex<Vec<ChatRequest>>,
        calls: AtomicUsize,
        fail_after: usize,
        delay: Option<Duration>,
        tick: Option<(Arc<TestClock>, Duration)>,
    }

    impl FakeSummarizer {
        fn new(reply: &str) -> Self {
            Self {
                reply: reply.to_string(),
                seen: StdMutex::new(Vec::new()),
                calls: AtomicUsize::new(0),
                fail_after: usize::MAX,
                delay: None,
                tick: None,
            }
        }
        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
        fn last_user_text(&self) -> String {
            let seen = self.seen.lock().expect("lock");
            let req = seen.last().expect("a request");
            match &req.messages[0] {
                ChatMessage::User(c) => match &c[0] {
                    UserContent::Text(t) => t.clone(),
                    UserContent::ToolResult(_) => String::new(),
                },
                ChatMessage::Assistant(_) => String::new(),
            }
        }
    }

    #[async_trait::async_trait]
    impl crate::provider::LlmProvider for FakeSummarizer {
        fn name(&self) -> &'static str {
            "fake-summarizer"
        }
        async fn send(&self, request: ChatRequest) -> Result<ChatResponse, ProviderError> {
            self.seen.lock().expect("lock").push(request);
            let n = self.calls.fetch_add(1, Ordering::SeqCst);
            if let Some(d) = self.delay {
                tokio::time::sleep(d).await;
            }
            if let Some((clock, by)) = &self.tick {
                clock.advance(*by);
            }
            if n >= self.fail_after {
                return Err(ProviderError::Transport("scripted failure".into()));
            }
            Ok(ChatResponse {
                content: vec![AssistantContent::Text(self.reply.clone())],
                stop_reason: StopReason::EndTurn,
                ..Default::default()
            })
        }
    }

    fn compactor_with(clock: SharedClock) -> Compactor {
        Compactor::new(
            clock,
            Model::try_from("claude-haiku-4-5").expect("catalog model"),
            MaxOutputTokens::try_from(1024).expect("max out"),
        )
    }

    fn long_messages(count: usize, chars_each: usize) -> Vec<ChatMessage> {
        (0..count)
            .map(|i| text_user(&format!("{i}:{}", "x".repeat(chars_each))))
            .collect()
    }

    #[tokio::test]
    async fn summarize_folds_once_and_records_a_sample() {
        let clock: SharedClock = Arc::new(TestClock::new());
        let compactor = compactor_with(clock.clone());
        let provider: SharedProvider = Arc::new(FakeSummarizer::new("ROLLING SUMMARY"));
        let deadline = clock.now() + Duration::from_mins(10);
        let mut samples = Vec::new();
        let out = compactor
            .summarize(
                &provider,
                None,
                long_messages(2, 100),
                deadline,
                &mut samples,
            )
            .await
            .expect("summary");
        assert!(out.as_str().contains("ROLLING SUMMARY"));
        assert_eq!(samples.len(), 1, "small overflow = one fold");
    }

    #[tokio::test]
    async fn summarize_feeds_previous_summary_forward() {
        // Stage 9: the rolling fold updates, not regenerates — prev is passed in.
        let clock: SharedClock = Arc::new(TestClock::new());
        let compactor = compactor_with(clock.clone());
        let fake = Arc::new(FakeSummarizer::new("UPDATED"));
        let provider: SharedProvider = fake.clone();
        let deadline = clock.now() + Duration::from_mins(10);
        let mut samples = Vec::new();
        compactor
            .summarize(
                &provider,
                Some("PRIOR-SUMMARY-XYZ"),
                long_messages(1, 50),
                deadline,
                &mut samples,
            )
            .await
            .expect("summary");
        assert!(fake.last_user_text().contains("PRIOR-SUMMARY-XYZ"));
        assert!(fake.last_user_text().contains("PREVIOUS SUMMARY"));
    }

    #[tokio::test]
    async fn summarize_caps_chunks_and_drops_oldest() {
        // Stage 12: more chunks than MAX_COMPACTION_CHUNKS -> oldest dropped,
        // fold loop bounded.
        let clock: SharedClock = Arc::new(TestClock::new());
        let compactor = compactor_with(clock.clone());
        let fake = Arc::new(FakeSummarizer::new("S"));
        let provider: SharedProvider = fake.clone();
        let deadline = clock.now() + Duration::from_mins(100);
        // Each message exceeds SUMMARIZER_INPUT_BUDGET tokens, so each is its own
        // chunk. MAX_COMPACTION_CHUNKS + 3 messages -> 3 oldest dropped.
        let big = (summarizer_input_budget() + 1_000) * CHARS_PER_TOKEN;
        let overflow = long_messages(MAX_COMPACTION_CHUNKS + 3, big);
        let mut samples = Vec::new();
        compactor
            .summarize(&provider, None, overflow, deadline, &mut samples)
            .await
            .expect("summary");
        assert_eq!(fake.calls(), MAX_COMPACTION_CHUNKS, "fold loop is bounded");
    }

    #[tokio::test(start_paused = true)]
    async fn summarize_times_out_per_fold() {
        let clock: SharedClock = Arc::new(TestClock::new());
        let compactor = compactor_with(clock.clone());
        let mut fake = FakeSummarizer::new("never");
        fake.delay = Some(COMPACTION_LLM_TIMEOUT * 2);
        let provider: SharedProvider = Arc::new(fake);
        let deadline = clock.now() + Duration::from_mins(100);
        let mut samples = Vec::new();
        let err = compactor
            .summarize(
                &provider,
                None,
                long_messages(1, 50),
                deadline,
                &mut samples,
            )
            .await
            .expect_err("must time out");
        assert!(matches!(err, CompactionError::SummarizerTimeout));
    }

    #[tokio::test]
    async fn summarize_stops_at_wall_clock_budget() {
        // Stage 11: each fold advances the clock past the deadline; the next
        // fold's guard trips before sending.
        let test_clock = Arc::new(TestClock::new());
        let clock: SharedClock = test_clock.clone();
        let compactor = compactor_with(clock.clone());
        let mut fake = FakeSummarizer::new("S");
        fake.tick = Some((test_clock.clone(), Duration::from_secs(100)));
        let provider: SharedProvider = Arc::new(fake);
        let deadline = clock.now() + Duration::from_secs(90); // < the 100s tick
        let big = (summarizer_input_budget() + 1_000) * CHARS_PER_TOKEN;
        let overflow = long_messages(3, big); // 3 chunks
        let mut samples = Vec::new();
        let err = compactor
            .summarize(&provider, None, overflow, deadline, &mut samples)
            .await
            .expect_err("wall clock");
        assert!(matches!(err, CompactionError::WallClockExceeded));
        assert_eq!(samples.len(), 1, "one fold ran before the budget tripped");
    }

    #[tokio::test]
    async fn summarize_failure_surfaces_provider_error() {
        let clock: SharedClock = Arc::new(TestClock::new());
        let compactor = compactor_with(clock.clone());
        let mut fake = FakeSummarizer::new("S");
        fake.fail_after = 0; // first call fails
        let provider: SharedProvider = Arc::new(fake);
        let deadline = clock.now() + Duration::from_mins(10);
        let mut samples = Vec::new();
        let err = compactor
            .summarize(
                &provider,
                None,
                long_messages(1, 50),
                deadline,
                &mut samples,
            )
            .await
            .expect_err("provider error");
        assert!(matches!(err, CompactionError::Provider(_)));
    }
}
