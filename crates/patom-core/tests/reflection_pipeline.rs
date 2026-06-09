//! P8: a reflection turn runs as background cognition — its LLM exchange lands
//! in `background_turn_messages`, never the chat feed.
//!
//! Drives the worker end-to-end: a background trigger is claimed, the agent
//! reads the seeded reflection prompt from the background store, replies, and
//! the worker marks it done — adding ZERO `thread_messages` rows.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;

use patom::agent_core::AgentBuilder;
use patom::agents::{
    AGENT_PROMPT_CACHE_CAP, AGENT_PROMPT_CACHE_TTL, AgentFactory, CachedAgents, SharedAgents,
};
use patom::auth::Caller;
use patom::background::{NewBackgroundMessage, PgBackgroundStore, SharedBackgroundStore};
use patom::clock::SystemClock;
use patom::colleagues::{resolve_agent_colleague, resolve_user_colleague};
use patom::hook::HookChain;
use patom::memory::{SharedMemory, StaticMemory};
use patom::provider::{
    AssistantContent, ChatMessage, ChatRequest, ChatResponse, LlmProvider, Model, ProviderError,
    ProviderId, ProviderRegistry, SharedProvider, SharedProviderRegistry, StopReason, UserContent,
};
use patom::runtime::{
    IdempotencyKey, NewTrigger, PgDagBudget, PgPromptQueue, PgResponseHub, RequestKindPayload,
    RequestStatus, SharedDagBudget, SharedPromptQueue, SharedResponseSink, WorkerConfig,
    WorkerPool,
};
use patom::threads::{MessageKind, NewMessage, PgThreadStore, SharedThreadStore};
use patom::tools::{ToolBox, ToolRegistry};
use sqlx::PgPool;
use uuid::Uuid;

mod common;
use common::pg::seed_tenant;

/// Provider that always replies with plain reflection text (no tools).
#[derive(Debug)]
struct AlwaysText;

#[async_trait]
impl LlmProvider for AlwaysText {
    fn name(&self) -> &'static str {
        "always-text"
    }
    async fn send(&self, _request: ChatRequest) -> Result<ChatResponse, ProviderError> {
        Ok(ChatResponse {
            content: vec![AssistantContent::Text(
                "reflected: nothing to remember".into(),
            )],
            stop_reason: StopReason::EndTurn,
            ..Default::default()
        })
    }
}

#[sqlx::test]
async fn reflection_writes_no_thread_message_rows(pool: PgPool) {
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
    let background: SharedBackgroundStore =
        Arc::new(PgBackgroundStore::new(pool.clone(), clock.clone()));
    let queue: SharedPromptQueue = Arc::new(PgPromptQueue::new(pool.clone(), clock.clone()));
    let dag: SharedDagBudget = Arc::new(PgDagBudget::new(pool.clone()));
    let sink: SharedResponseSink = Arc::new(PgResponseHub::new(pool.clone(), clock.clone()));

    // A conversation thread with one posted message — the slice the reflection
    // is "about" (its id is the frozen up_to_message_id).
    let thread = threads
        .create_thread(&caller, None, None, human, Some(agent_col))
        .await
        .expect("thread");
    let convo_msg = threads
        .append(
            &caller,
            thread,
            NewMessage {
                kind: MessageKind::Posted,
                sender: Some(human),
                owner_agent_id: None,
                receiver: Some(agent_col),
                body: ChatMessage::User(vec![UserContent::Text("the meeting is at noon".into())]),
                request_id: None,
                idempotency_key: None,
            },
        )
        .await
        .expect("seed conversation");

    // The scheduler seeds the background turn with the reflection prompt.
    let turn = background
        .create_turn(&caller, seed.agent_id)
        .await
        .expect("create background turn");
    background
        .append(
            &caller,
            turn,
            NewBackgroundMessage {
                sender: None,
                body: ChatMessage::User(vec![UserContent::Text(
                    "Reflect on the conversation above.".into(),
                )]),
                request_id: None,
            },
        )
        .await
        .expect("seed reflection prompt");

    // Agent factory: background store wired, always-text provider, no tools.
    let model = Model::try_from("test-model").expect("catalog");
    let shared: SharedProvider = Arc::new(AlwaysText);
    let providers: SharedProviderRegistry = Arc::new(
        ProviderRegistry::builder()
            .insert(ProviderId::Anthropic, shared)
            .build(),
    );
    let memory: SharedMemory = Arc::new(StaticMemory::new("test"));
    let model_f = model;
    let providers_f = providers;
    let memory_f = memory;
    let background_f = background.clone();
    let threads_f = threads.clone();
    let clock_f = clock.clone();
    let factory: AgentFactory = Arc::new(move |_record| {
        AgentBuilder::new(providers_f.clone(), memory_f.clone(), model_f)
            .expect("builder")
            .with_clock(clock_f.clone())
            .with_thread_store(threads_f.clone())
            .with_background_store(background_f.clone())
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

    // A background reflection trigger (claim_key = the background turn).
    let trigger = queue
        .enqueue_trigger(NewTrigger {
            org_id: seed.org_id,
            acting_user_id: seed.user_id,
            thread_id: None,
            state_id: None,
            background_turn_id: Some(turn),
            sender_colleague_id: agent_col,
            receiver_agent_id: seed.agent_id,
            root_request_id: None,
            trigger_message_id: None,
            idempotency_key: IdempotencyKey::try_from(format!("reflect-{}", Uuid::new_v4()))
                .expect("key"),
            kind_payload: RequestKindPayload::Reflection {
                thread_id: thread,
                up_to_message_id: convo_msg,
            },
        })
        .await
        .expect("enqueue reflection trigger");

    let mut done = false;
    for _ in 0..200u32 {
        let view = queue.status(trigger).await.expect("status");
        if view.status == RequestStatus::Done {
            done = true;
            break;
        }
        assert!(
            view.status != RequestStatus::Failed,
            "reflection turn must not fail: {:?}",
            view.failure_reason
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    workers.shutdown().await;
    assert!(done, "reflection trigger reached Done");

    // Headline invariant: the reflection added NO chat-feed rows — only the one
    // conversation message we seeded remains.
    let (thread_rows,): (i64,) =
        sqlx::query_as("SELECT count(*) FROM thread_messages WHERE thread_id = $1")
            .bind(thread)
            .fetch_one(&pool)
            .await
            .expect("count thread_messages");
    assert_eq!(
        thread_rows, 1,
        "reflection must add no thread_messages rows (only the seeded conversation message)"
    );

    // The cognition landed in the background turn: seeded prompt + the reply.
    let (bg_rows,): (i64,) =
        sqlx::query_as("SELECT count(*) FROM background_turn_messages WHERE turn_id = $1")
            .bind(turn)
            .fetch_one(&pool)
            .await
            .expect("count background_turn_messages");
    assert!(
        bg_rows >= 2,
        "the reflection exchange is recorded in the background turn, got {bg_rows}"
    );

    // The checkpoint advanced to the frozen slice's `up_to_message_id`, so the
    // scheduler won't re-enqueue this idle window next tick.
    let checkpoint: Option<(uuid::Uuid,)> = sqlx::query_as(
        "SELECT last_message_id FROM reflection_checkpoints WHERE agent_id = $1 AND thread_id = $2",
    )
    .bind(seed.agent_id)
    .bind(thread)
    .fetch_optional(&pool)
    .await
    .expect("read reflection checkpoint");
    assert_eq!(
        checkpoint.map(|(id,)| id),
        Some(convo_msg.as_uuid()),
        "successful reflection advances the checkpoint to up_to_message_id",
    );
}
