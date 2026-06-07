//! End-to-end agent tests against a fake `LlmProvider` and a fake `Tool`.
//!
//! Proves that the agent loop is fully provider-agnostic: nothing about Anthropic,
//! reqwest, or claudius appears here. Plug in any backend and it works.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use patom::agent_core::AgentBuilder;
use patom::clock::SystemClock;
use patom::hook::HookChain;
use patom::memory::{SharedMemory, StaticMemory};
use patom::provider::{
    AssistantContent, ChatRequest, ChatResponse, LlmProvider, Model, ProviderError, ProviderId,
    ProviderRegistry, SharedProvider, SharedProviderRegistry, StopReason, ToolCall, ToolCallId,
};
use patom::runtime::PromptRequestId;
use patom::session::{PgSessionStore, SharedSessionStore};
use patom::tools::{SharedTool, Tool, ToolCallContext, ToolError, ToolRegistry};
use patom::types::{Prompt, ToolName};
use sqlx::PgPool;

mod common;
use common::pg::{human_to_agent_session, seed_prompt_request, seed_tenant};

/// Create a fresh human-to-default-agent session and a stub `prompt_requests`
/// row bound to it. Returns both ids — `agent.reply` needs the request_id and
/// `session_messages.request_id` FK-references it on every append.
async fn fresh_session(
    pool: &PgPool,
    seed: &common::pg::Seed,
) -> (patom::session::SessionId, PromptRequestId) {
    let store = PgSessionStore::new(pool.clone(), SystemClock::shared());
    let session =
        human_to_agent_session(pool, &store, seed.agent_id, seed.org_id, seed.user_id).await;
    let request = seed_prompt_request(pool, session, seed.agent_id, seed.org_id).await;
    (session, request)
}

/// Provider that returns a pre-scripted sequence of responses, one per turn. Records the
/// requests it sees so tests can assert on them.
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

    fn calls(&self) -> usize {
        self.cursor.load(Ordering::SeqCst)
    }

    fn last_request(&self) -> ChatRequest {
        self.seen
            .lock()
            .unwrap()
            .last()
            .cloned()
            .expect("no requests recorded")
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

/// Tool that records every input it receives and returns a fixed reply.
#[derive(Debug)]
struct CountingTool {
    name: ToolName,
    schema: Arc<Value>,
    calls: AtomicUsize,
}

impl CountingTool {
    fn new(name: &str) -> Self {
        Self {
            name: ToolName::try_from(name).expect("valid name"),
            schema: Arc::new(json!({"type": "object"})),
            calls: AtomicUsize::new(0),
        }
    }

    fn count(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl Tool for CountingTool {
    fn name(&self) -> &ToolName {
        &self.name
    }
    fn description(&self) -> &str {
        "test tool"
    }
    fn input_schema(&self) -> Arc<Value> {
        self.schema.clone()
    }
    async fn execute(&self, _input: Value, _ctx: &ToolCallContext) -> Result<String, ToolError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok("tool ran".into())
    }
}

fn text_response(s: &str, stop: StopReason) -> ChatResponse {
    ChatResponse {
        content: vec![AssistantContent::Text(s.into())],
        stop_reason: stop,
        ..Default::default()
    }
}

fn tool_call_response(name: &str, id: &str) -> ChatResponse {
    ChatResponse {
        content: vec![AssistantContent::ToolCall(ToolCall {
            id: ToolCallId::try_from(id).expect("valid"),
            name: ToolName::try_from(name).expect("valid"),
            input: json!({}),
        })],
        stop_reason: StopReason::ToolUse,
        ..Default::default()
    }
}

fn build(
    pool: &PgPool,
    seed: &common::pg::Seed,
    provider: Arc<ScriptedProvider>,
    tools: Vec<SharedTool>,
) -> patom::Agent {
    build_with_model(
        pool,
        seed,
        provider,
        tools,
        Model::try_from("test-model").expect("catalog"),
    )
}

fn build_with_model(
    pool: &PgPool,
    seed: &common::pg::Seed,
    provider: Arc<ScriptedProvider>,
    tools: Vec<SharedTool>,
    model: Model,
) -> patom::Agent {
    let provider: SharedProvider = provider;
    let clock = SystemClock::shared();
    let sessions: SharedSessionStore = Arc::new(PgSessionStore::new(pool.clone(), clock));
    let memory: SharedMemory = Arc::new(StaticMemory::new("test prompt"));
    let providers: SharedProviderRegistry = Arc::new(
        ProviderRegistry::builder()
            .insert(model.provider(), provider)
            .build(),
    );
    let mut builder = ToolRegistry::builder();
    for t in tools {
        builder.register(t);
    }
    let _ = seed; // ids accessed via seed at call sites; pool drives sessions
    AgentBuilder::new(providers, sessions, memory, model)
        .expect("builder")
        .with_builtin_tools(builder.build())
        .with_hooks(HookChain::new())
        .build()
}

#[sqlx::test]
async fn returns_text_when_no_tool_call(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let provider = Arc::new(ScriptedProvider::new(vec![text_response(
        "hi back",
        StopReason::EndTurn,
    )]));
    let agent = build(&pool, &seed, provider.clone(), vec![]);

    let (session, request_id) = fresh_session(&pool, &seed).await;
    let prompt = Prompt::try_from("hello").expect("prompt");
    let reply = agent
        .reply(
            session,
            common::pg::agent_participant(&pool, seed.org_id, seed.agent_id).await,
            vec![prompt],
            request_id,
            patom::auth::Caller::new(seed.user_id, seed.org_id),
            patom::runtime::RequestKindPayload::Normal {},
            CancellationToken::new(),
            None,
        )
        .await
        .expect("reply");

    assert_eq!(reply.final_text(), "hi back");
    assert_eq!(provider.calls(), 1);
}

#[sqlx::test]
async fn runs_tool_then_returns_text(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let provider = Arc::new(ScriptedProvider::new(vec![
        tool_call_response("counter", "call-1"),
        text_response("done", StopReason::EndTurn),
    ]));
    let counter = Arc::new(CountingTool::new("counter"));
    let agent = build(&pool, &seed, provider.clone(), vec![counter.clone()]);

    let (session, request_id) = fresh_session(&pool, &seed).await;
    let prompt = Prompt::try_from("use the tool").expect("prompt");
    let reply = agent
        .reply(
            session,
            common::pg::agent_participant(&pool, seed.org_id, seed.agent_id).await,
            vec![prompt],
            request_id,
            patom::auth::Caller::new(seed.user_id, seed.org_id),
            patom::runtime::RequestKindPayload::Normal {},
            CancellationToken::new(),
            None,
        )
        .await
        .expect("reply");

    assert_eq!(reply.final_text(), "done");
    assert_eq!(counter.count(), 1, "tool should have been invoked once");
    assert_eq!(provider.calls(), 2, "two turns: tool call, then final");
}

#[sqlx::test]
async fn unknown_tool_does_not_loop_forever(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let provider = Arc::new(ScriptedProvider::new(vec![
        tool_call_response("missing-tool", "call-x"),
        text_response("recovered", StopReason::EndTurn),
    ]));
    let agent = build(&pool, &seed, provider.clone(), vec![]);

    let (session, request_id) = fresh_session(&pool, &seed).await;
    let prompt = Prompt::try_from("try the missing tool").expect("prompt");
    let reply = agent
        .reply(
            session,
            common::pg::agent_participant(&pool, seed.org_id, seed.agent_id).await,
            vec![prompt],
            request_id,
            patom::auth::Caller::new(seed.user_id, seed.org_id),
            patom::runtime::RequestKindPayload::Normal {},
            CancellationToken::new(),
            None,
        )
        .await
        .expect("reply");

    assert_eq!(reply.final_text(), "recovered");
}

#[sqlx::test]
async fn cancellation_short_circuits(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let provider = Arc::new(ScriptedProvider::new(vec![text_response(
        "never used",
        StopReason::EndTurn,
    )]));
    let agent = build(&pool, &seed, provider, vec![]);

    let (session, request_id) = fresh_session(&pool, &seed).await;
    let prompt = Prompt::try_from("cancel me").expect("prompt");
    let cancel = CancellationToken::new();
    cancel.cancel();

    let err = agent
        .reply(
            session,
            common::pg::agent_participant(&pool, seed.org_id, seed.agent_id).await,
            vec![prompt],
            request_id,
            patom::auth::Caller::new(seed.user_id, seed.org_id),
            patom::runtime::RequestKindPayload::Normal {},
            cancel,
            None,
        )
        .await
        .expect_err("cancelled");
    matches!(err, patom::AgentError::Cancelled);
}

#[sqlx::test]
async fn provider_specs_match_registered_tools(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let provider = Arc::new(ScriptedProvider::new(vec![text_response(
        "ok",
        StopReason::EndTurn,
    )]));
    let counter = Arc::new(CountingTool::new("counter"));
    let agent = build(&pool, &seed, provider.clone(), vec![counter]);

    let (session, request_id) = fresh_session(&pool, &seed).await;
    let prompt = Prompt::try_from("hi").expect("prompt");
    let _ = agent
        .reply(
            session,
            common::pg::agent_participant(&pool, seed.org_id, seed.agent_id).await,
            vec![prompt],
            request_id,
            patom::auth::Caller::new(seed.user_id, seed.org_id),
            patom::runtime::RequestKindPayload::Normal {},
            CancellationToken::new(),
            None,
        )
        .await
        .expect("reply");

    let req = provider.last_request();
    assert_eq!(req.tools.len(), 1);
    assert_eq!(req.tools[0].name.as_str(), "counter");
}

#[sqlx::test]
async fn agent_carries_its_model_into_the_provider_call(pool: PgPool) {
    // Per-agent model selection: the model the agent was constructed with
    // should land verbatim in `ChatRequest::model`. Sanity-checks the
    // Agent::call_provider routing path under the new closed-catalog model
    // type — request.model.as_str() must match the catalog name.
    let seed = seed_tenant(&pool).await;
    let provider = Arc::new(ScriptedProvider::new(vec![text_response(
        "ok",
        StopReason::EndTurn,
    )]));
    let model = Model::try_from("test-model-openai").expect("catalog");
    let agent = build_with_model(&pool, &seed, provider.clone(), vec![], model);

    let (session, request_id) = fresh_session(&pool, &seed).await;
    let prompt = Prompt::try_from("hi").expect("prompt");
    let _ = agent
        .reply(
            session,
            common::pg::agent_participant(&pool, seed.org_id, seed.agent_id).await,
            vec![prompt],
            request_id,
            patom::auth::Caller::new(seed.user_id, seed.org_id),
            patom::runtime::RequestKindPayload::Normal {},
            CancellationToken::new(),
            None,
        )
        .await
        .expect("reply");

    let req = provider.last_request();
    assert_eq!(req.model.as_str(), "test-model-openai");
    assert_eq!(req.model.provider(), ProviderId::Openai);
}

#[sqlx::test]
async fn registry_routes_model_to_its_catalog_provider(pool: PgPool) {
    // Two scripted providers registered under different ProviderIds: only
    // the one matching the model's catalog provider should be called. The
    // other one staying at zero calls proves we're routing, not just
    // grabbing the first thing in the registry.
    let seed = seed_tenant(&pool).await;
    let anthropic_provider = Arc::new(ScriptedProvider::new(vec![text_response(
        "anthropic-served",
        StopReason::EndTurn,
    )]));
    let openai_provider = Arc::new(ScriptedProvider::new(vec![text_response(
        "openai-served",
        StopReason::EndTurn,
    )]));
    let model = Model::try_from("test-model-openai").expect("catalog");
    let anthropic_shared: SharedProvider = anthropic_provider.clone();
    let openai_shared: SharedProvider = openai_provider.clone();
    let providers: SharedProviderRegistry = Arc::new(
        ProviderRegistry::builder()
            .insert(ProviderId::Anthropic, anthropic_shared)
            .insert(ProviderId::Openai, openai_shared)
            .build(),
    );
    let clock = SystemClock::shared();
    let sessions: SharedSessionStore = Arc::new(PgSessionStore::new(pool.clone(), clock));
    let memory: SharedMemory = Arc::new(StaticMemory::new("test prompt"));
    let agent = AgentBuilder::new(providers, sessions, memory, model)
        .expect("builder")
        .with_builtin_tools(ToolRegistry::empty())
        .with_hooks(HookChain::new())
        .build();

    let (session, request_id) = fresh_session(&pool, &seed).await;
    let prompt = Prompt::try_from("hi").expect("prompt");
    let reply = agent
        .reply(
            session,
            common::pg::agent_participant(&pool, seed.org_id, seed.agent_id).await,
            vec![prompt],
            request_id,
            patom::auth::Caller::new(seed.user_id, seed.org_id),
            patom::runtime::RequestKindPayload::Normal {},
            CancellationToken::new(),
            None,
        )
        .await
        .expect("reply");

    assert_eq!(reply.final_text(), "openai-served");
    assert_eq!(openai_provider.calls(), 1);
    assert_eq!(
        anthropic_provider.calls(),
        0,
        "wrong provider was routed to"
    );
}
