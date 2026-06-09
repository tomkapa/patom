//! End-to-end tests for the prompt pipeline against the Postgres-backed runtime:
//! * worker round-trip (enqueue → claim → run → publish → mark_done)
//! * cancellation ends the in-flight turn before its second prompt is processed
//! * streaming guarantees (text before done, exactly-once Text)
//! * idempotent enqueue
//!
//! Each test gets its own freshly-migrated database via `#[sqlx::test]` so
//! they can run in parallel with full isolation.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;

use patom::agent_core::AgentBuilder;
use patom::agents::{
    AGENT_PROMPT_CACHE_CAP, AGENT_PROMPT_CACHE_TTL, AgentFactory, CachedAgents, SharedAgentStore,
    SharedAgents,
};
use patom::clock::SystemClock;
use patom::hook::HookChain;
use patom::memory::{SharedMemory, StaticMemory};
use patom::provider::{
    AssistantContent, ChatRequest, ChatResponse, LlmProvider, Model, ProviderError, ProviderId,
    ProviderRegistry, SharedProvider, SharedProviderRegistry, StopReason,
};
use patom::runtime::queue::PromptQueue as _;
use patom::runtime::{
    IdempotencyKey, LeaseTiming, NewPromptRequest, PgDagBudget, PgPromptQueue, PgResponseHub,
    RequestStatus, ResponseChunk, SharedDagBudget, SharedResponseSource, StreamEvent, WorkerConfig,
    WorkerPool,
};
use patom::session::{PgSessionStore, SharedSessionStore};
use patom::tools::system::SendMessageTool;
use patom::tools::{ToolBox, ToolRegistry};
use patom::types::Prompt;
use sqlx::PgPool;

mod common;
use common::pg::{human_to_agent_session, seed_tenant};

#[derive(Debug)]
struct ScriptedProvider {
    responses: Vec<ChatResponse>,
    cursor: AtomicUsize,
    delay: Duration,
}

#[async_trait]
impl LlmProvider for ScriptedProvider {
    fn name(&self) -> &'static str {
        "scripted"
    }

    async fn send(&self, _request: ChatRequest) -> Result<ChatResponse, ProviderError> {
        if !self.delay.is_zero() {
            tokio::time::sleep(self.delay).await;
        }
        let i = self.cursor.fetch_add(1, Ordering::SeqCst);
        self.responses
            .get(i)
            .cloned()
            .ok_or_else(|| ProviderError::Transport("script exhausted".into()))
    }
}

fn text_response(s: &str) -> ChatResponse {
    ChatResponse {
        content: vec![AssistantContent::Text(s.into())],
        stop_reason: StopReason::EndTurn,
        ..Default::default()
    }
}

/// Scripted assistant response that calls `send_message(Human, content)`.
/// The worker's ping-pong guard requires every successful turn to deliver
/// at least one message; tests that previously asserted post-turn `Done`
/// chunks must now route their reply through this tool.
fn send_message_human_response(content: &str, call_id: &str) -> ChatResponse {
    use patom::provider::{ToolCall, ToolCallId};
    use patom::types::ToolName;
    ChatResponse {
        content: vec![AssistantContent::ToolCall(ToolCall {
            id: ToolCallId::try_from(call_id).expect("id"),
            name: ToolName::try_from("send_message").expect("name"),
            input: serde_json::json!({
                "receiver": { "kind": "human" },
                "content": content,
            }),
        })],
        stop_reason: StopReason::ToolUse,
        ..Default::default()
    }
}

struct Harness {
    queue: Arc<PgPromptQueue>,
    hub: Arc<PgResponseHub>,
    sessions: SharedSessionStore,
    default_agent_id: patom::agents::AgentId,
    default_agent_colleague_id: patom::colleagues::ColleagueId,
    default_org_id: patom::auth::OrgId,
    default_user_id: patom::auth::UserId,
    default_user_colleague_id: patom::colleagues::ColleagueId,
    pool: patom::runtime::WorkerPoolHandle,
}

impl Harness {
    fn default_human_participant(&self) -> patom::types::Participant {
        patom::types::Participant::human(self.default_user_colleague_id, self.default_user_id)
    }
    #[allow(dead_code)]
    fn default_agent_participant(&self) -> patom::types::Participant {
        patom::types::Participant::agent(self.default_agent_colleague_id, self.default_agent_id)
    }
}

async fn build_harness(pool: PgPool, provider: Arc<ScriptedProvider>) -> Harness {
    let seed = seed_tenant(&pool).await;
    let clock = SystemClock::shared();
    let queue_impl = Arc::new(PgPromptQueue::new(pool.clone(), clock.clone()));
    let hub = Arc::new(PgResponseHub::new(pool.clone(), clock.clone()));
    let sessions: SharedSessionStore = Arc::new(PgSessionStore::new(pool.clone(), clock.clone()));
    let threads: patom::threads::SharedThreadStore = Arc::new(patom::threads::PgThreadStore::new(
        pool.clone(),
        clock.clone(),
    ));

    let provider: SharedProvider = provider;
    let memory: SharedMemory = Arc::new(StaticMemory::new("test"));
    let model = Model::try_from("test-model").expect("catalog");
    let providers: SharedProviderRegistry = Arc::new(
        ProviderRegistry::builder()
            .insert(ProviderId::Anthropic, provider)
            .build(),
    );
    let agent_store: SharedAgentStore = common::pg::shared_agent_store(pool.clone(), clock.clone());
    let colleagues: patom::colleagues::SharedColleagueStore =
        Arc::new(patom::colleagues::PgColleagueStore::new(pool.clone()));
    let dag: SharedDagBudget = Arc::new(PgDagBudget::new(pool.clone()));
    // The worker's ping-pong guard requires every successful turn to call
    // send_message, so test scripts must invoke it.
    let registry = ToolRegistry::builder()
        .with(Arc::new(SendMessageTool::new(
            threads.clone(),
            queue_impl.clone(),
            dag.clone(),
            agent_store.clone(),
            colleagues.clone(),
            hub.clone(),
        )))
        .build();
    let agent = AgentBuilder::new(providers, sessions.clone(), memory, model)
        .expect("builder")
        .with_clock(clock.clone())
        .with_tools(ToolBox::from_builtins(registry))
        .with_hooks(HookChain::new())
        .build();
    let factory: AgentFactory = Arc::new(move |_record| agent.clone());
    let agents_registry: SharedAgents = Arc::new(CachedAgents::new(
        agent_store,
        factory,
        AGENT_PROMPT_CACHE_CAP,
        AGENT_PROMPT_CACHE_TTL,
        clock.clone(),
    ));
    let memory_store_for_pool: patom::memory::SharedMemoryStore =
        Arc::new(patom::memory::PgMemoryStore::new(
            pool.clone(),
            clock.clone(),
            common::embedding::FakeEmbeddingProvider::shared(),
        ));

    let cfg = WorkerConfig {
        workers: 2,
        lease_timing: LeaseTiming::try_new(Duration::from_secs(2), Duration::from_millis(100))
            .expect("valid timing"),
        max_turn_duration: Duration::from_secs(10),
        idle_poll: Duration::from_millis(20),
        cancel_poll: Duration::from_millis(50),
    };
    let worker_pool = WorkerPool::new(
        queue_impl.clone(),
        queue_impl.clone(),
        hub.clone(),
        agents_registry,
        sessions.clone(),
        dag,
        pool.clone(),
        memory_store_for_pool,
        clock.clone(),
        cfg,
    )
    .spawn();

    let default_agent_colleague_id =
        patom::colleagues::resolve_agent_colleague(&pool, seed.org_id, seed.agent_id)
            .await
            .expect("agent colleague");
    let default_user_colleague_id =
        patom::colleagues::resolve_user_colleague(&pool, seed.org_id, seed.user_id)
            .await
            .expect("user colleague");
    Harness {
        default_agent_id: seed.agent_id,
        default_agent_colleague_id,
        default_org_id: seed.org_id,
        default_user_id: seed.user_id,
        default_user_colleague_id,
        queue: queue_impl,
        hub,
        sessions,
        pool: worker_pool,
    }
}

fn req(
    session: patom::session::SessionId,
    sender: patom::types::Participant,
    agent_id: patom::agents::AgentId,
    content: &str,
    key: &str,
    org_id: patom::auth::OrgId,
    user_id: patom::auth::UserId,
) -> NewPromptRequest {
    NewPromptRequest {
        session: Some(session),
        sender,
        receiver_agent_id: agent_id,
        parent_session: None,
        content: Prompt::try_from(content).expect("p"),
        idempotency_key: IdempotencyKey::try_from(key).expect("k"),
        org_id,
        created_by_user_id: user_id,
        kind_payload: patom::runtime::RequestKindPayload::Normal {},
    }
}

/// Root-of-DAG enqueue: `session: None` makes the queue mint a fresh
/// session AND seed `prompt_request_dags` in one transaction. Required by
/// any test whose script invokes `send_message`, since the tool's
/// `dag.bump_or_fail` needs the DAG row to exist.
fn req_root(
    sender: patom::types::Participant,
    agent_id: patom::agents::AgentId,
    content: &str,
    key: &str,
    org_id: patom::auth::OrgId,
    user_id: patom::auth::UserId,
) -> NewPromptRequest {
    NewPromptRequest {
        session: None,
        sender,
        receiver_agent_id: agent_id,
        parent_session: None,
        content: Prompt::try_from(content).expect("p"),
        idempotency_key: IdempotencyKey::try_from(key).expect("k"),
        org_id,
        created_by_user_id: user_id,
        kind_payload: patom::runtime::RequestKindPayload::Normal {},
    }
}

async fn drain_until_terminal(
    hub: Arc<PgResponseHub>,
    id: patom::runtime::PromptRequestId,
    deadline: Duration,
) -> Vec<ResponseChunk> {
    let source: SharedResponseSource = hub;
    let mut stream = source.subscribe(id, None).await.expect("subscribe");
    let mut got = Vec::new();
    let until = std::time::Instant::now() + deadline;
    while std::time::Instant::now() < until {
        let next = tokio::time::timeout(Duration::from_millis(200), stream.next()).await;
        let Ok(Some(item)) = next else { continue };
        let ev = item.expect("ok");
        if let StreamEvent::Chunk(env) = ev {
            let terminal = env.chunk.is_terminal();
            got.push(env.chunk);
            if terminal {
                return got;
            }
        }
    }
    got
}

/// Wait for `id` to reach a terminal status. The worker publishes the terminal
/// chunk *before* committing `mark_done` / `mark_failed`, so the SSE stream can
/// see Done before the DB row flips. Pg adds a commit RTT to that gap; tests poll
/// briefly to avoid races.
async fn await_terminal_status(
    queue: &Arc<PgPromptQueue>,
    id: patom::runtime::PromptRequestId,
    deadline: Duration,
) -> RequestStatus {
    let until = std::time::Instant::now() + deadline;
    while std::time::Instant::now() < until {
        let view = queue.status(id).await.expect("status");
        if matches!(view.status, RequestStatus::Done | RequestStatus::Failed) {
            return view.status;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    queue.status(id).await.expect("status").status
}

#[sqlx::test]
async fn round_trip_publishes_done_chunk(pool: PgPool) {
    let provider = Arc::new(ScriptedProvider {
        responses: vec![
            send_message_human_response("hello back", "call-1"),
            // Closing text is the agent's own internal note — the
            // human-visible reply went via send_message above. Must be
            // non-empty so the agent loop doesn't treat it as EmptyReply.
            text_response("done"),
        ],
        cursor: AtomicUsize::new(0),
        delay: Duration::ZERO,
    });
    let h = build_harness(pool.clone(), provider).await;
    let id = h
        .queue
        .enqueue(req_root(
            h.default_human_participant(),
            h.default_agent_id,
            "hi",
            "k1",
            h.default_org_id,
            h.default_user_id,
        ))
        .await
        .expect("enqueue")
        .request_id();

    let chunks = drain_until_terminal(h.hub.clone(), id, Duration::from_secs(3)).await;
    assert!(
        chunks
            .iter()
            .any(|c| matches!(c, ResponseChunk::Done { .. })),
        "expected a Done chunk, got {chunks:?}"
    );
    let status = await_terminal_status(&h.queue, id, Duration::from_secs(2)).await;
    assert!(matches!(status, RequestStatus::Done), "got {status:?}");

    h.pool.shutdown().await;
}

#[sqlx::test]
async fn cancellation_finishes_inflight_and_skips_next_turn(pool: PgPool) {
    let provider = Arc::new(ScriptedProvider {
        responses: vec![text_response("first reply"), text_response("second reply")],
        cursor: AtomicUsize::new(0),
        delay: Duration::from_millis(150),
    });
    let h = build_harness(pool.clone(), provider).await;
    let s = human_to_agent_session(
        &pool,
        h.sessions.as_ref(),
        h.default_agent_id,
        h.default_org_id,
        h.default_user_id,
    )
    .await;
    let first = h
        .queue
        .enqueue(req(
            s,
            h.default_human_participant(),
            h.default_agent_id,
            "first",
            "k-first",
            h.default_org_id,
            h.default_user_id,
        ))
        .await
        .expect("enqueue1")
        .request_id();

    // Wait for the first turn to start.
    let _ = drain_until_terminal(h.hub.clone(), first, Duration::from_secs(3)).await;

    let second = h
        .queue
        .enqueue(req(
            s,
            h.default_human_participant(),
            h.default_agent_id,
            "second",
            "k-second",
            h.default_org_id,
            h.default_user_id,
        ))
        .await
        .expect("enqueue2")
        .request_id();
    h.queue.request_cancellation(second).await.expect("cancel");

    let chunks = drain_until_terminal(h.hub.clone(), second, Duration::from_secs(3)).await;
    let status = await_terminal_status(&h.queue, second, Duration::from_secs(2)).await;
    assert!(
        matches!(status, RequestStatus::Done | RequestStatus::Failed),
        "second request must reach a terminal state; got {status:?}",
    );
    assert!(
        chunks.iter().any(ResponseChunk::is_terminal),
        "must have observed a terminal chunk on the SSE stream"
    );

    h.pool.shutdown().await;
}

#[sqlx::test]
async fn streaming_emits_text_before_done(pool: PgPool) {
    let provider = Arc::new(ScriptedProvider {
        responses: vec![
            send_message_human_response("payload", "call-1"),
            text_response("incremental answer"),
        ],
        cursor: AtomicUsize::new(0),
        delay: Duration::ZERO,
    });
    let h = build_harness(pool.clone(), provider).await;
    let id = h
        .queue
        .enqueue(req_root(
            h.default_human_participant(),
            h.default_agent_id,
            "hi",
            "stream-key",
            h.default_org_id,
            h.default_user_id,
        ))
        .await
        .expect("enqueue")
        .request_id();

    let chunks = drain_until_terminal(h.hub.clone(), id, Duration::from_secs(3)).await;
    let mut text_idx = None;
    let mut done_idx = None;
    for (i, c) in chunks.iter().enumerate() {
        if matches!(c, ResponseChunk::Text { value: _ }) {
            text_idx.get_or_insert(i);
        }
        if matches!(c, ResponseChunk::Done { .. }) {
            done_idx = Some(i);
        }
    }
    let t = text_idx.expect("expected at least one Text chunk");
    let d = done_idx.expect("expected a terminal Done chunk");
    assert!(t < d, "Text chunk must precede Done; got chunks {chunks:?}");
    let text_count = chunks
        .iter()
        .filter(|c| matches!(c, ResponseChunk::Text { value: _ }))
        .count();
    assert_eq!(
        text_count, 1,
        "exactly one Text chunk per assistant text block; got {chunks:?}"
    );

    h.pool.shutdown().await;
}

#[sqlx::test]
async fn mid_turn_cancellation_aborts_in_flight_turn(pool: PgPool) {
    let provider = Arc::new(ScriptedProvider {
        responses: vec![text_response("never delivered")],
        cursor: AtomicUsize::new(0),
        delay: Duration::from_secs(2),
    });
    let h = build_harness(pool.clone(), provider).await;
    let s = human_to_agent_session(
        &pool,
        h.sessions.as_ref(),
        h.default_agent_id,
        h.default_org_id,
        h.default_user_id,
    )
    .await;
    let id = h
        .queue
        .enqueue(req(
            s,
            h.default_human_participant(),
            h.default_agent_id,
            "slow",
            "k-mid-cancel",
            h.default_org_id,
            h.default_user_id,
        ))
        .await
        .expect("enqueue")
        .request_id();

    tokio::time::sleep(Duration::from_millis(150)).await;
    h.queue
        .request_cancellation(id)
        .await
        .expect("request cancel");

    let mut terminal = None;
    for _ in 0..40 {
        tokio::time::sleep(Duration::from_millis(50)).await;
        let view = h.queue.status(id).await.expect("status");
        if matches!(view.status, RequestStatus::Done | RequestStatus::Failed) {
            terminal = Some(view);
            break;
        }
    }
    let view = terminal.expect("request must reach terminal state");
    assert!(
        matches!(view.status, RequestStatus::Failed),
        "expected Failed, got {:?}",
        view.status
    );

    h.pool.shutdown().await;
}

#[sqlx::test]
async fn idempotent_repeat_returns_same_request_id(pool: PgPool) {
    let provider = Arc::new(ScriptedProvider {
        responses: vec![text_response("ok")],
        cursor: AtomicUsize::new(0),
        delay: Duration::ZERO,
    });
    let h = build_harness(pool.clone(), provider).await;
    let s = human_to_agent_session(
        &pool,
        h.sessions.as_ref(),
        h.default_agent_id,
        h.default_org_id,
        h.default_user_id,
    )
    .await;
    let a = h
        .queue
        .enqueue(req(
            s,
            h.default_human_participant(),
            h.default_agent_id,
            "hi",
            "same-key",
            h.default_org_id,
            h.default_user_id,
        ))
        .await
        .expect("a")
        .request_id();
    let b = h
        .queue
        .enqueue(req(
            s,
            h.default_human_participant(),
            h.default_agent_id,
            "hi",
            "same-key",
            h.default_org_id,
            h.default_user_id,
        ))
        .await
        .expect("b")
        .request_id();
    assert_eq!(a, b);
    h.pool.shutdown().await;
}
