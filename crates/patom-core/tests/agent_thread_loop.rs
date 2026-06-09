//! P4: the agent reads its thread context AT RUN TIME.
//!
//! The thread model drops the per-turn `prompts` argument — when the worker
//! claims a `(thread, agent)` turn, the agent reads the thread tail from the
//! feed store itself (`ThreadStore::context_for_agent`). This proves the
//! provider sees the pre-seeded human post even though nothing was passed into
//! the reply call.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use patom::agent_core::AgentBuilder;
use patom::auth::Caller;
use patom::clock::SystemClock;
use patom::colleagues::{resolve_agent_colleague, resolve_user_colleague};
use patom::memory::{SharedMemory, StaticMemory};
use patom::provider::{
    AssistantContent, ChatMessage, ChatRequest, ChatResponse, LlmProvider, Model, ProviderError,
    ProviderRegistry, SharedProvider, SharedProviderRegistry, StopReason, UserContent,
};
use patom::runtime::{IdempotencyKey, NewTrigger, PgPromptQueue, RequestKindPayload};
use patom::session::{PgSessionStore, SharedSessionStore};
use patom::threads::{MessageKind, NewMessage, PgThreadStore, SharedThreadStore};
use patom::tools::ToolRegistry;
use sqlx::PgPool;
use uuid::Uuid;

mod common;
use common::pg::{agent_participant, seed_tenant};

/// Provider that records every request it sees and replays a scripted answer.
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

fn text_response(s: &str, stop: StopReason) -> ChatResponse {
    ChatResponse {
        content: vec![AssistantContent::Text(s.into())],
        stop_reason: stop,
        ..Default::default()
    }
}

#[sqlx::test]
async fn context_is_read_at_run(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let clock = SystemClock::shared();
    let threads: SharedThreadStore = Arc::new(PgThreadStore::new(pool.clone(), clock.clone()));
    let queue = PgPromptQueue::new(pool.clone(), clock.clone());
    let caller = Caller::new(seed.user_id, seed.org_id);
    let human = resolve_user_colleague(&pool, seed.org_id, seed.user_id)
        .await
        .expect("human colleague");
    let agent_col = resolve_agent_colleague(&pool, seed.org_id, seed.agent_id)
        .await
        .expect("agent colleague");

    let thread = threads
        .create_thread(&caller, None, None, human)
        .await
        .expect("create thread");
    let state = threads
        .resolve_participation(&caller, thread, seed.agent_id)
        .await
        .expect("participation");

    // Pre-seed the feed with a human post. Nothing about this text is passed
    // into the reply call — the agent must read it from the store at run time.
    threads
        .append(
            &caller,
            thread,
            NewMessage {
                kind: MessageKind::Posted,
                sender: Some(human),
                owner_agent_id: None,
                receiver: Some(agent_col),
                body: ChatMessage::User(vec![UserContent::Text(
                    "please remember the number 42".into(),
                )]),
                request_id: None,
            },
        )
        .await
        .expect("seed human post");

    // A trigger row so the agent's appended artifacts carry a valid request_id.
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
        .expect("enqueue trigger");

    let provider = Arc::new(ScriptedProvider::new(vec![text_response(
        "acknowledged",
        StopReason::EndTurn,
    )]));
    let model = Model::try_from("test-model").expect("catalog model");
    let shared: SharedProvider = provider.clone();
    let providers: SharedProviderRegistry = Arc::new(
        ProviderRegistry::builder()
            .insert(model.provider(), shared)
            .build(),
    );
    let sessions: SharedSessionStore = Arc::new(PgSessionStore::new(pool.clone(), clock.clone()));
    let memory: SharedMemory = Arc::new(StaticMemory::new("test prompt"));
    let agent = AgentBuilder::new(providers, sessions, memory, model)
        .expect("builder")
        .with_thread_store(threads.clone())
        .with_builtin_tools(ToolRegistry::empty())
        .build();

    let viewer = agent_participant(&pool, seed.org_id, seed.agent_id).await;
    let reply = agent
        .reply_in_thread(
            state,
            thread,
            viewer,
            request_id,
            caller,
            RequestKindPayload::Normal {},
            CancellationToken::new(),
            None,
        )
        .await
        .expect("reply_in_thread");

    let req = provider.last_request();
    let saw_42 = req.messages.iter().any(|m| {
        matches!(m, ChatMessage::User(blocks)
            if blocks.iter().any(|b| matches!(b, UserContent::Text(t) if t.contains("42"))))
    });
    assert!(
        saw_42,
        "agent must read the human post from the thread feed at run time"
    );
    assert_eq!(reply.final_text(), "acknowledged");
}
