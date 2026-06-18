//! #185 end-to-end: a heavy tool result is reduced at produce time — it enters
//! the agent's context bounded and marked-partial, while its full body is
//! offloaded to `tool_artifacts` and remains recoverable byte-for-byte. Nothing
//! is destructively lost. Companion to the compaction loop (#182).

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use patom::agent_core::AgentBuilder;
use patom::agents::AgentId;
use patom::auth::{Caller, OrgId};
use patom::clock::SystemClock;
use patom::colleagues::{resolve_agent_colleague, resolve_user_colleague};
use patom::memory::{SharedMemory, StaticMemory};
use patom::provider::{
    AssistantContent, ChatMessage, ChatRequest, ChatResponse, LlmProvider, Model, ProviderError,
    ProviderRegistry, SharedProvider, SharedProviderRegistry, StopReason, ToolCall, ToolCallId,
    UserContent,
};
use patom::runtime::{
    IdempotencyKey, NewTrigger, PgPromptQueue, PromptQueue, PromptRequestId, RequestKindPayload,
};
use patom::threads::{
    AgentThreadId, ArtifactHandle, ArtifactSelector, MessageKind, NewMessage, PgThreadStore,
    SharedThreadStore, ThreadId,
};
use patom::tools::{
    ReductionIntent, SharedTool, Tool, ToolCallContext, ToolError, ToolRegistry, ToolResultPolicy,
};
use patom::types::ToolName;
use sqlx::PgPool;
use uuid::Uuid;

mod common;
use common::pg::{agent_participant, seed_tenant};

/// A tool whose result is large enough to trip the produce-time reduction
/// threshold. Uses the default policy (Paginate) — no LLM, lossless.
#[derive(Debug)]
struct BigResultTool {
    name: ToolName,
    schema: Arc<Value>,
    body: String,
}

impl BigResultTool {
    fn new(body: String) -> Self {
        Self {
            name: ToolName::try_from("big_result").expect("name"),
            schema: Arc::new(
                json!({"type": "object", "properties": {}, "additionalProperties": false}),
            ),
            body,
        }
    }
}

#[async_trait]
impl Tool for BigResultTool {
    fn name(&self) -> &ToolName {
        &self.name
    }
    fn description(&self) -> &str {
        "returns a large body"
    }
    fn input_schema(&self) -> Arc<Value> {
        self.schema.clone()
    }
    async fn execute(&self, _input: Value, _ctx: &ToolCallContext) -> Result<String, ToolError> {
        Ok(self.body.clone())
    }
}

/// Like `BigResultTool`, but declares a `Summarize` policy — its oversized
/// result is reduced by the cheap-model extractive fold, not paginated.
#[derive(Debug)]
struct SummarizeBigTool {
    name: ToolName,
    schema: Arc<Value>,
    body: String,
}

#[async_trait]
impl Tool for SummarizeBigTool {
    fn name(&self) -> &ToolName {
        &self.name
    }
    fn description(&self) -> &str {
        "returns a large body, summarized"
    }
    fn input_schema(&self) -> Arc<Value> {
        self.schema.clone()
    }
    fn result_policy(&self, _input: &Value) -> ToolResultPolicy {
        ToolResultPolicy::Summarize {
            intent: ReductionIntent::clamp("extract the key facts".to_owned()),
        }
    }
    async fn execute(&self, _input: Value, _ctx: &ToolCallContext) -> Result<String, ToolError> {
        Ok(self.body.clone())
    }
}

#[derive(Debug)]
struct ScriptedProvider {
    script: Vec<ChatResponse>,
    cursor: AtomicUsize,
    seen: std::sync::Mutex<Vec<ChatRequest>>,
}

impl ScriptedProvider {
    fn new(script: Vec<ChatResponse>) -> Self {
        Self {
            script,
            cursor: AtomicUsize::new(0),
            seen: std::sync::Mutex::new(Vec::new()),
        }
    }
    fn last_request(&self) -> ChatRequest {
        self.seen
            .lock()
            .unwrap()
            .last()
            .cloned()
            .expect("a request")
    }
}

#[async_trait]
impl LlmProvider for ScriptedProvider {
    fn name(&self) -> &'static str {
        "scripted"
    }
    async fn send(&self, request: ChatRequest) -> Result<ChatResponse, ProviderError> {
        self.seen.lock().unwrap().push(request);
        let i = self.cursor.fetch_add(1, Ordering::SeqCst);
        self.script
            .get(i)
            .cloned()
            .ok_or_else(|| ProviderError::Transport("script exhausted".into()))
    }
}

fn tool_call_response(call_id: &str) -> ChatResponse {
    tool_call_to(call_id, "big_result")
}

fn tool_call_to(call_id: &str, tool: &str) -> ChatResponse {
    ChatResponse {
        content: vec![AssistantContent::ToolCall(ToolCall {
            id: ToolCallId::try_from(call_id).expect("call id"),
            name: ToolName::try_from(tool).expect("name"),
            input: json!({}),
        })],
        stop_reason: StopReason::ToolUse,
        ..Default::default()
    }
}

fn text_response(s: &str) -> ChatResponse {
    ChatResponse {
        content: vec![AssistantContent::Text(s.into())],
        stop_reason: StopReason::EndTurn,
        ..Default::default()
    }
}

/// A response with no usable content — makes a summarizer fold fail, so the
/// reducer must degrade to the lossless paginate preview.
fn empty_response() -> ChatResponse {
    ChatResponse {
        content: vec![],
        stop_reason: StopReason::EndTurn,
        ..Default::default()
    }
}

fn first_tool_result_output(req: &ChatRequest) -> Option<String> {
    for m in &req.messages {
        if let ChatMessage::User(blocks) = m {
            for b in blocks {
                if let UserContent::ToolResult(r) = b {
                    return Some(r.output.clone());
                }
            }
        }
    }
    None
}

/// A `(thread, agent)` ready to drive one reduction turn, plus the seeded
/// human post and enqueued trigger. Extracted so each test stays well under the
/// 70-line ceiling (CLAUDE.md §4) and the setup lives in one place.
struct Fixture {
    pool: PgPool,
    threads: SharedThreadStore,
    caller: Caller,
    org_id: OrgId,
    agent_id: AgentId,
    thread: ThreadId,
    state: AgentThreadId,
    request_id: PromptRequestId,
}

/// Seed a tenant + thread, post `prompt` as the human, and enqueue the trigger.
async fn setup(pool: PgPool, prompt: &str) -> Fixture {
    let clock = SystemClock::shared();
    let threads: SharedThreadStore = Arc::new(PgThreadStore::new(pool.clone(), clock.clone()));
    let queue = PgPromptQueue::new(pool.clone(), clock.clone());
    let seed = seed_tenant(&pool).await;
    let caller = Caller::new(seed.user_id, seed.org_id);
    let human = resolve_user_colleague(&pool, seed.org_id, seed.user_id)
        .await
        .expect("human");
    let agent_col = resolve_agent_colleague(&pool, seed.org_id, seed.agent_id)
        .await
        .expect("agent col");
    let thread = threads
        .create_thread(&caller, None, None, human, Some(agent_col))
        .await
        .expect("thread");
    let state = threads
        .resolve_participation(&caller, thread, seed.agent_id)
        .await
        .expect("participation");
    // The turn reads the feed at run time; seed a human post so it has context.
    threads
        .append(
            &caller,
            thread,
            NewMessage {
                kind: MessageKind::Posted,
                sender: Some(human),
                owner_agent_id: None,
                receiver: Some(agent_col),
                body: ChatMessage::User(vec![UserContent::Text(prompt.into())]),
                request_id: None,
                idempotency_key: None,
            },
        )
        .await
        .expect("seed human post");
    let request_id = queue
        .enqueue_trigger(NewTrigger {
            org_id: seed.org_id,
            acting_user_id: seed.user_id,
            thread_id: Some(thread),
            state_id: Some(state),
            background_turn_id: None,
            sender_colleague_id: human,
            receiver_agent_id: seed.agent_id,
            root_request_id: None,
            trigger_message_id: None,
            idempotency_key: IdempotencyKey::try_from(format!("tag-{}", Uuid::new_v4()))
                .expect("key"),
            kind_payload: RequestKindPayload::Normal {},
        })
        .await
        .expect("trigger");
    Fixture {
        pool,
        threads,
        caller,
        org_id: seed.org_id,
        agent_id: seed.agent_id,
        thread,
        state,
        request_id,
    }
}

/// Build an agent with `tool` registered and the test model routed to a scripted
/// provider, run one turn, and return the tool_result body the model saw next.
async fn run_turn(fx: &Fixture, tool: SharedTool, script: Vec<ChatResponse>) -> String {
    let provider = Arc::new(ScriptedProvider::new(script));
    let model = Model::try_from("test-model").expect("catalog model");
    let shared: SharedProvider = provider.clone();
    let providers: SharedProviderRegistry = Arc::new(
        ProviderRegistry::builder()
            .insert(model.provider(), shared)
            .build(),
    );
    let memory: SharedMemory = Arc::new(StaticMemory::new("test prompt"));
    let registry = ToolRegistry::builder().with(tool).build();
    let agent = AgentBuilder::new(providers, memory, model)
        .expect("builder")
        .with_thread_store(fx.threads.clone())
        .with_builtin_tools(registry)
        .build();
    let viewer = agent_participant(&fx.pool, fx.org_id, fx.agent_id).await;
    agent
        .reply_in_thread(
            fx.state,
            fx.thread,
            viewer,
            fx.request_id,
            fx.request_id,
            fx.caller,
            RequestKindPayload::Normal {},
            CancellationToken::new(),
            None,
        )
        .await
        .expect("reply_in_thread");
    first_tool_result_output(&provider.last_request()).expect("tool_result in context")
}

/// Read a page of the offloaded artifact back, for "nothing is lost" assertions.
async fn recover(fx: &Fixture, handle: &ArtifactHandle, offset: usize, limit: usize) -> String {
    fx.threads
        .load_tool_artifact_slice(fx.org_id, handle, ArtifactSelector::Page { offset, limit })
        .await
        .expect("load")
        .expect("artifact present")
        .into_string()
}

/// A `Summarize`-policy tool wrapped as a `SharedTool`.
fn summarize_tool(body: String) -> SharedTool {
    Arc::new(SummarizeBigTool {
        name: ToolName::try_from("summarize_big").expect("name"),
        schema: Arc::new(
            json!({"type": "object", "properties": {}, "additionalProperties": false}),
        ),
        body,
    })
}

/// #185: a heavy result enters context bounded + marked-partial (paginate,
/// zero-LLM), and its full body is recoverable byte-for-byte.
#[sqlx::test]
async fn heavy_tool_result_is_reduced_and_recoverable(pool: PgPool) {
    let fx = setup(pool, "fetch the big thing").await;
    // Distinct head/tail markers prove the preview keeps both ends.
    let big = format!("HEADSTART{}TAILFINISH", "x".repeat(50_000));
    let handle = ArtifactHandle::content_address(&big);

    let script = vec![tool_call_response("call-1"), text_response("done")];
    let seen = run_turn(&fx, Arc::new(BigResultTool::new(big.clone())), script).await;

    assert!(
        seen.chars().count() < big.chars().count(),
        "reduced below the raw body"
    );
    assert!(seen.starts_with("HEADSTART"), "head preserved");
    assert!(seen.contains("TAILFINISH"), "tail preserved (lossless win)");
    assert!(seen.contains("chars omitted"), "marked partial (anti-lie)");
    assert!(
        seen.contains(handle.as_str()),
        "carries the recovery handle"
    );
    assert!(
        seen.contains("read_artifact"),
        "tells the model how to recover"
    );

    assert_eq!(recover(&fx, &handle, 0, 9).await, "HEADSTART");
    let total = big.chars().count();
    assert_eq!(
        recover(&fx, &handle, total - 10, 10).await,
        "TAILFINISH",
        "the offloaded tail is recoverable"
    );
}

/// #185 stage 10: a `Summarize`-policy tool runs the cheap-model extractive fold
/// (cheapest usable model — Anthropic Haiku, routed to the test provider); the
/// visible body is the summary marked NOT-full + handle.
#[sqlx::test]
async fn heavy_tool_result_is_summarized(pool: PgPool) {
    let fx = setup(pool, "summarize the big thing").await;
    let big = format!("PAYLOAD{}END", "y".repeat(50_000));
    let handle = ArtifactHandle::content_address(&big);

    // Tool call → reducer's single fold returns the extract → main turn ends.
    let script = vec![
        tool_call_to("call-1", "summarize_big"),
        text_response("GIST-OF-THE-RESULT"),
        text_response("done"),
    ];
    let seen = run_turn(&fx, summarize_tool(big.clone()), script).await;

    assert!(
        seen.contains("GIST-OF-THE-RESULT"),
        "summary body present: {seen}"
    );
    assert!(seen.contains("NOT"), "flagged as not the full result");
    assert!(seen.contains(handle.as_str()), "carries the handle");
    assert!(seen.contains("read_artifact"), "recovery path present");
    assert!(
        !seen.contains(&"y".repeat(200)),
        "the raw payload is not inlined"
    );

    assert_eq!(recover(&fx, &handle, 0, 7).await, "PAYLOAD");
}

/// #185 stage 12: when the summarizer fold fails, the reducer degrades to the
/// lossless paginate preview — never a blind clip; the full body is recoverable.
#[sqlx::test]
async fn summarizer_failure_degrades_to_lossless_preview(pool: PgPool) {
    let fx = setup(pool, "go").await;
    let big = format!("PAYLOAD{}END", "y".repeat(50_000));
    let handle = ArtifactHandle::content_address(&big);

    // The single fold attempt returns nothing usable → summarize fails → degrade.
    let script = vec![
        tool_call_to("call-1", "summarize_big"),
        empty_response(),
        text_response("done"),
    ];
    let seen = run_turn(&fx, summarize_tool(big.clone()), script).await;

    assert!(seen.starts_with("PAYLOAD"), "head preserved on degrade");
    assert!(
        seen.contains("chars omitted"),
        "lossless preview, not a summary"
    );
    assert!(seen.contains(handle.as_str()), "handle still carried");
    assert!(
        !seen.contains("Summarized tool result"),
        "no summary marker — the fold failed"
    );

    assert_eq!(recover(&fx, &handle, 0, 7).await, "PAYLOAD");
}
