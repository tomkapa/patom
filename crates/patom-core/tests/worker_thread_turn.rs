//! P6: the worker drives a thread-feed turn through the force-exit guard.
//!
//! An agent that produces text without calling `send_message` lands no posted
//! row (no egress); the worker nudges and retries, and after
//! `MAX_PINGPONG_RETRIES` parks the trigger as `Failed(NoEgress)`.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;

use patom::agent_core::AgentBuilder;
use patom::agents::{
    AGENT_PROMPT_CACHE_CAP, AGENT_PROMPT_CACHE_TTL, AgentFactory, CachedAgents, SharedAgents,
};
use patom::auth::Caller;
use patom::clock::SystemClock;
use patom::colleagues::{resolve_agent_colleague, resolve_user_colleague};
use patom::hook::HookChain;
use patom::memory::{SharedMemory, StaticMemory};
use patom::provider::{
    AssistantContent, ChatMessage, ChatRequest, ChatResponse, LlmProvider, Model, ProviderError,
    ProviderId, ProviderRegistry, SharedProvider, SharedProviderRegistry, StopReason, UserContent,
};
use patom::runtime::{
    FailureReason, IdempotencyKey, NewTrigger, PgDagBudget, PgPromptQueue, PgResponseHub,
    RequestKindPayload, SharedDagBudget, SharedPromptQueue, SharedResponseSink, WorkerConfig,
    WorkerPool,
};
use patom::session::{PgSessionStore, SharedSessionStore};
use patom::threads::{MessageKind, NewMessage, PgThreadStore, SharedThreadStore};
use patom::tools::{ToolBox, ToolRegistry};
use sqlx::PgPool;
use uuid::Uuid;

mod common;
use common::pg::seed_tenant;

/// Provider that always replies with plain text — never a tool call — so the
/// agent never reaches `send_message` and the ping-pong guard always fires.
#[derive(Debug)]
struct AlwaysText;

#[async_trait]
impl LlmProvider for AlwaysText {
    fn name(&self) -> &'static str {
        "always-text"
    }

    async fn send(&self, _request: ChatRequest) -> Result<ChatResponse, ProviderError> {
        Ok(ChatResponse {
            content: vec![AssistantContent::Text("just thinking out loud".into())],
            stop_reason: StopReason::EndTurn,
            ..Default::default()
        })
    }
}

#[sqlx::test]
async fn no_egress_nudges_then_fails(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let clock = SystemClock::shared();
    let caller = Caller::new(seed.user_id, seed.org_id);
    let human = resolve_user_colleague(&pool, seed.org_id, seed.user_id)
        .await
        .expect("human colleague");
    let agent_col = resolve_agent_colleague(&pool, seed.org_id, seed.agent_id)
        .await
        .expect("agent colleague");

    let threads: SharedThreadStore = Arc::new(PgThreadStore::new(pool.clone(), clock.clone()));
    let queue: SharedPromptQueue = Arc::new(PgPromptQueue::new(pool.clone(), clock.clone()));
    let dag: SharedDagBudget = Arc::new(PgDagBudget::new(pool.clone()));
    let sink: SharedResponseSink = Arc::new(PgResponseHub::new(pool.clone(), clock.clone()));

    // A thread with a human post tagging the agent (the context the turn reads).
    let thread = threads
        .create_thread(&caller, None, None, human)
        .await
        .expect("thread");
    threads
        .append(
            &caller,
            thread,
            NewMessage {
                kind: MessageKind::Posted,
                sender: Some(human),
                owner_agent_id: None,
                receiver: Some(agent_col),
                body: ChatMessage::User(vec![UserContent::Text("are you there?".into())]),
                request_id: None,
            },
        )
        .await
        .expect("seed human post");
    let state = threads
        .resolve_participation(&caller, thread, seed.agent_id)
        .await
        .expect("participation");

    // Agent factory: always-text provider, thread store wired, empty toolbox.
    let model = Model::try_from("test-model").expect("catalog");
    let shared: SharedProvider = Arc::new(AlwaysText);
    let providers: SharedProviderRegistry = Arc::new(
        ProviderRegistry::builder()
            .insert(ProviderId::Anthropic, shared)
            .build(),
    );
    let sessions: SharedSessionStore = Arc::new(PgSessionStore::new(pool.clone(), clock.clone()));
    let memory: SharedMemory = Arc::new(StaticMemory::new("test"));
    let model_for_factory = model;
    let threads_for_factory = threads.clone();
    let clock_for_factory = clock.clone();
    let factory: AgentFactory = Arc::new(move |_record| {
        AgentBuilder::new(
            providers.clone(),
            sessions.clone(),
            memory.clone(),
            model_for_factory,
        )
        .expect("builder")
        .with_clock(clock_for_factory.clone())
        .with_thread_store(threads_for_factory.clone())
        .with_tools(ToolBox::from_builtins(ToolRegistry::empty()))
        .with_hooks(HookChain::new())
        .build()
    });
    let agents: SharedAgents = Arc::new(CachedAgents::new(
        common::pg::shared_agent_store(pool.clone(), clock.clone()),
        factory,
        AGENT_PROMPT_CACHE_CAP,
        AGENT_PROMPT_CACHE_TTL,
        clock.clone(),
    ));

    let cfg = WorkerConfig {
        workers: 1,
        max_turn_duration: Duration::from_secs(10),
        idle_poll: Duration::from_millis(20),
        cancel_poll: Duration::from_millis(50),
        ..WorkerConfig::default()
    };
    let workers = WorkerPool::new(queue.clone(), sink, agents, threads.clone(), dag, cfg).spawn();

    // A human-@tag trigger waking the agent in this thread (root mint).
    let trigger = queue
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
        .expect("enqueue trigger");

    // Poll until the trigger reaches a terminal status.
    let mut terminal = None;
    for _ in 0..200u32 {
        let view = queue.status(trigger).await.expect("status");
        if view.status.is_terminal() {
            terminal = Some(view);
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    workers.shutdown().await;

    let view = terminal.expect("trigger reached a terminal status");
    assert!(
        matches!(view.failure_reason, Some(FailureReason::NoEgress)),
        "a turn that never posts must fail with NoEgress, got {:?}",
        view.failure_reason
    );
}
