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

use patom_rs::agent_core::AgentBuilder;
use patom_rs::clock::SystemClock;
use patom_rs::hook::HookChain;
use patom_rs::memory::{SharedMemory, StaticMemory};
use patom_rs::provider::{
    AssistantContent, ChatRequest, ChatResponse, LlmProvider, Model, ProviderError, ProviderId,
    ProviderRegistry, SharedProvider, SharedProviderRegistry, StopReason, ToolCall, ToolCallId,
};
use patom_rs::runtime::PromptRequestId;
use patom_rs::session::{PgSessionStore, SharedSessionStore};
use patom_rs::tools::{SharedTool, Tool, ToolCallContext, ToolError, ToolRegistry};
use patom_rs::types::{Participant, Prompt, ToolName};

mod common;
use common::pg::{TestDb, human_to_agent_session, seed_prompt_request};

/// Create a fresh human-to-default-agent session and a stub `prompt_requests`
/// row bound to it. Returns both ids — `agent.reply` needs the request_id and
/// `session_messages.request_id` FK-references it on every append.
async fn fresh_session(db: &TestDb) -> (patom_rs::session::SessionId, PromptRequestId) {
    let store = PgSessionStore::new(db.pool.clone(), SystemClock::shared());
    let session = human_to_agent_session(
        &store,
        db.default_agent_id,
        db.default_org_id,
        db.default_user_id,
    )
    .await;
    let request =
        seed_prompt_request(&db.pool, session, db.default_agent_id, db.default_org_id).await;
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

fn build(db: &TestDb, provider: Arc<ScriptedProvider>, tools: Vec<SharedTool>) -> patom_rs::Agent {
    build_with_model(
        db,
        provider,
        tools,
        Model::try_from("test-model").expect("catalog"),
    )
}

fn build_with_model(
    db: &TestDb,
    provider: Arc<ScriptedProvider>,
    tools: Vec<SharedTool>,
    model: Model,
) -> patom_rs::Agent {
    let provider: SharedProvider = provider;
    let clock = SystemClock::shared();
    let sessions: SharedSessionStore = Arc::new(PgSessionStore::new(db.pool.clone(), clock));
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
    AgentBuilder::new(providers, sessions, memory, model)
        .expect("builder")
        .with_builtin_tools(builder.build())
        .with_hooks(HookChain::new())
        .build()
}

#[tokio::test(flavor = "multi_thread")]
async fn returns_text_when_no_tool_call() {
    let db = TestDb::fresh().await;
    let provider = Arc::new(ScriptedProvider::new(vec![text_response(
        "hi back",
        StopReason::EndTurn,
    )]));
    let agent = build(&db, provider.clone(), vec![]);

    let (session, request_id) = fresh_session(&db).await;
    let prompt = Prompt::try_from("hello").expect("prompt");
    let reply = agent
        .reply(
            session,
            Participant::agent(db.default_agent_id),
            vec![prompt],
            request_id,
            patom_rs::auth::Caller::new(db.default_user_id, db.default_org_id),
            patom_rs::runtime::RequestKindPayload::Normal {},
            CancellationToken::new(),
            None,
        )
        .await
        .expect("reply");

    assert_eq!(reply.final_text(), "hi back");
    assert_eq!(provider.calls(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn runs_tool_then_returns_text() {
    let db = TestDb::fresh().await;
    let provider = Arc::new(ScriptedProvider::new(vec![
        tool_call_response("counter", "call-1"),
        text_response("done", StopReason::EndTurn),
    ]));
    let counter = Arc::new(CountingTool::new("counter"));
    let agent = build(&db, provider.clone(), vec![counter.clone()]);

    let (session, request_id) = fresh_session(&db).await;
    let prompt = Prompt::try_from("use the tool").expect("prompt");
    let reply = agent
        .reply(
            session,
            Participant::agent(db.default_agent_id),
            vec![prompt],
            request_id,
            patom_rs::auth::Caller::new(db.default_user_id, db.default_org_id),
            patom_rs::runtime::RequestKindPayload::Normal {},
            CancellationToken::new(),
            None,
        )
        .await
        .expect("reply");

    assert_eq!(reply.final_text(), "done");
    assert_eq!(counter.count(), 1, "tool should have been invoked once");
    assert_eq!(provider.calls(), 2, "two turns: tool call, then final");
}

#[tokio::test(flavor = "multi_thread")]
async fn unknown_tool_does_not_loop_forever() {
    let db = TestDb::fresh().await;
    let provider = Arc::new(ScriptedProvider::new(vec![
        tool_call_response("missing-tool", "call-x"),
        text_response("recovered", StopReason::EndTurn),
    ]));
    let agent = build(&db, provider.clone(), vec![]);

    let (session, request_id) = fresh_session(&db).await;
    let prompt = Prompt::try_from("try the missing tool").expect("prompt");
    let reply = agent
        .reply(
            session,
            Participant::agent(db.default_agent_id),
            vec![prompt],
            request_id,
            patom_rs::auth::Caller::new(db.default_user_id, db.default_org_id),
            patom_rs::runtime::RequestKindPayload::Normal {},
            CancellationToken::new(),
            None,
        )
        .await
        .expect("reply");

    assert_eq!(reply.final_text(), "recovered");
}

#[tokio::test(flavor = "multi_thread")]
async fn cancellation_short_circuits() {
    let db = TestDb::fresh().await;
    let provider = Arc::new(ScriptedProvider::new(vec![text_response(
        "never used",
        StopReason::EndTurn,
    )]));
    let agent = build(&db, provider, vec![]);

    let (session, request_id) = fresh_session(&db).await;
    let prompt = Prompt::try_from("cancel me").expect("prompt");
    let cancel = CancellationToken::new();
    cancel.cancel();

    let err = agent
        .reply(
            session,
            Participant::agent(db.default_agent_id),
            vec![prompt],
            request_id,
            patom_rs::auth::Caller::new(db.default_user_id, db.default_org_id),
            patom_rs::runtime::RequestKindPayload::Normal {},
            cancel,
            None,
        )
        .await
        .expect_err("cancelled");
    matches!(err, patom_rs::AgentError::Cancelled);
}

#[tokio::test(flavor = "multi_thread")]
async fn provider_specs_match_registered_tools() {
    let db = TestDb::fresh().await;
    let provider = Arc::new(ScriptedProvider::new(vec![text_response(
        "ok",
        StopReason::EndTurn,
    )]));
    let counter = Arc::new(CountingTool::new("counter"));
    let agent = build(&db, provider.clone(), vec![counter]);

    let (session, request_id) = fresh_session(&db).await;
    let prompt = Prompt::try_from("hi").expect("prompt");
    let _ = agent
        .reply(
            session,
            Participant::agent(db.default_agent_id),
            vec![prompt],
            request_id,
            patom_rs::auth::Caller::new(db.default_user_id, db.default_org_id),
            patom_rs::runtime::RequestKindPayload::Normal {},
            CancellationToken::new(),
            None,
        )
        .await
        .expect("reply");

    let req = provider.last_request();
    assert_eq!(req.tools.len(), 1);
    assert_eq!(req.tools[0].name.as_str(), "counter");
}

#[tokio::test(flavor = "multi_thread")]
async fn agent_carries_its_model_into_the_provider_call() {
    // Per-agent model selection: the model the agent was constructed with
    // should land verbatim in `ChatRequest::model`. Sanity-checks the
    // Agent::call_provider routing path under the new closed-catalog model
    // type — request.model.as_str() must match the catalog name.
    let db = TestDb::fresh().await;
    let provider = Arc::new(ScriptedProvider::new(vec![text_response(
        "ok",
        StopReason::EndTurn,
    )]));
    let model = Model::try_from("test-model-openai").expect("catalog");
    let agent = build_with_model(&db, provider.clone(), vec![], model);

    let (session, request_id) = fresh_session(&db).await;
    let prompt = Prompt::try_from("hi").expect("prompt");
    let _ = agent
        .reply(
            session,
            Participant::agent(db.default_agent_id),
            vec![prompt],
            request_id,
            patom_rs::auth::Caller::new(db.default_user_id, db.default_org_id),
            patom_rs::runtime::RequestKindPayload::Normal {},
            CancellationToken::new(),
            None,
        )
        .await
        .expect("reply");

    let req = provider.last_request();
    assert_eq!(req.model.as_str(), "test-model-openai");
    assert_eq!(req.model.provider(), ProviderId::Openai);
}

#[tokio::test(flavor = "multi_thread")]
async fn registry_routes_model_to_its_catalog_provider() {
    // Two scripted providers registered under different ProviderIds: only
    // the one matching the model's catalog provider should be called. The
    // other one staying at zero calls proves we're routing, not just
    // grabbing the first thing in the registry.
    let db = TestDb::fresh().await;
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
    let sessions: SharedSessionStore = Arc::new(PgSessionStore::new(db.pool.clone(), clock));
    let memory: SharedMemory = Arc::new(StaticMemory::new("test prompt"));
    let agent = AgentBuilder::new(providers, sessions, memory, model)
        .expect("builder")
        .with_builtin_tools(ToolRegistry::empty())
        .with_hooks(HookChain::new())
        .build();

    let (session, request_id) = fresh_session(&db).await;
    let prompt = Prompt::try_from("hi").expect("prompt");
    let reply = agent
        .reply(
            session,
            Participant::agent(db.default_agent_id),
            vec![prompt],
            request_id,
            patom_rs::auth::Caller::new(db.default_user_id, db.default_org_id),
            patom_rs::runtime::RequestKindPayload::Normal {},
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
