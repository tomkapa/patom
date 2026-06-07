//! End-to-end scenario test for the full multi-agent communication stack.
//!
//! Plays out a realistic delegation: a human asks the **Coordinator** agent
//! (the seeded default) to translate a phrase. The Coordinator does not know
//! French, so it delegates to a **Translator** agent via `send_message`.
//! The Translator replies with the translation via `send_message(Agent(coordinator))`.
//! The Coordinator forwards the result to the human via `send_message(Human)`.
//! When the DAG drains, the worker emits a terminal `Done` chunk on the root
//! request's SSE stream.
//!
//! What this exercises end-to-end:
//! - Step 1 — schema, `Participant`, viewer-mapped session storage.
//! - Step 2 — `Agents` registry with a per-agent factory: each agent gets
//!   its own scripted provider, picked by `AgentRecord::id` at build time.
//! - Step 3 — DAG budget seeded on the human POST; bumped four times across
//!   the conversation, well under cap.
//! - Step 4 — `send_message` agent-receiver branch (Coordinator → Translator
//!   and Translator → Coordinator). Each agent-receiver send also publishes
//!   a best-effort `AgentMessage` on the DAG stream so the handoff is visible
//!   to observers (Slack pump, web SSE) — not just the final reply.
//! - Step 6 — `send_message(Human)` publishes `AgentMessage` on the root
//!   stream.
//! - Step 7 — every turn calls `send_message`, so the ping-pong guard
//!   never trips.
//! - Step 8 — once every `prompt_requests` row in the DAG flips to `done`,
//!   the worker emits a terminal `Done` chunk on the root stream and
//!   closes it.
//!
//! Scripted-provider scripts are short and explicit: each agent emits a
//! `send_message` tool call followed by a benign closing text. The closing
//! text is a private thought (per the system-prompt directive) and is not
//! delivered anywhere; only `send_message` content lands.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use serde_json::json;

use patom::agent_core::AgentBuilder;
use patom::agents::{
    AGENT_PROMPT_CACHE_CAP, AGENT_PROMPT_CACHE_TTL, AgentFactory, AgentId, AgentName,
    AgentSystemPrompt, CachedAgents, NewAgent, SharedAgentStore, SharedAgents,
};
use patom::clock::SystemClock;
use patom::hook::HookChain;
use patom::memory::{SharedMemory, StaticMemory};
use patom::provider::{
    AssistantContent, ChatRequest, ChatResponse, LlmProvider, Model, ProviderError, ProviderId,
    ProviderRegistry, SharedProvider, SharedProviderRegistry, StopReason, ToolCall, ToolCallId,
};
use patom::runtime::queue::PromptQueue as _;
use patom::runtime::{
    IdempotencyKey, LeaseTiming, NewPromptRequest, PgDagBudget, PgPromptQueue, PgResponseHub,
    PgThreadStream, PromptRequestId, RequestStatus, ResponseChunk, SharedDagBudget,
    SharedThreadStream, ThreadStreamEvent, WorkerConfig, WorkerPool, WorkerPoolHandle,
};
use patom::session::{PgSessionStore, SharedSessionStore};
use patom::tools::system::SendMessageTool;
use patom::tools::{ToolBox, ToolRegistry};
use patom::types::{Prompt, ToolName};
use sqlx::PgPool;
use tokio_util::sync::CancellationToken;

mod common;
use common::pg::seed_tenant;

/// Provider that dispatches to a per-agent script keyed by [`AgentId`].
///
/// Each Agent built by the registry's factory closure holds its own
/// [`AgentBoundProvider`] that internally points to a single agent's
/// scripted responses; the agent's `send` calls advance only that script.
#[derive(Debug)]
struct AgentBoundProvider {
    name: &'static str,
    responses: Vec<ChatResponse>,
    cursor: AtomicUsize,
}

impl AgentBoundProvider {
    fn new(name: &'static str, responses: Vec<ChatResponse>) -> Arc<Self> {
        Arc::new(Self {
            name,
            responses,
            cursor: AtomicUsize::new(0),
        })
    }
    fn calls(&self) -> usize {
        self.cursor.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl LlmProvider for AgentBoundProvider {
    fn name(&self) -> &'static str {
        self.name
    }
    async fn send(&self, _request: ChatRequest) -> Result<ChatResponse, ProviderError> {
        let i = self.cursor.fetch_add(1, Ordering::SeqCst);
        self.responses
            .get(i)
            .cloned()
            .ok_or_else(|| ProviderError::Transport(format!("script exhausted for {}", self.name)))
    }
}

/// One scripted assistant response that calls `send_message` against the
/// given receiver (`Human` or `Agent`).
fn send_message_chunk(receiver: serde_json::Value, content: &str, call_id: &str) -> ChatResponse {
    ChatResponse {
        content: vec![AssistantContent::ToolCall(ToolCall {
            id: ToolCallId::try_from(call_id).expect("id"),
            name: ToolName::try_from("send_message").expect("name"),
            input: json!({ "receiver": receiver, "content": content }),
        })],
        stop_reason: StopReason::ToolUse,
        ..Default::default()
    }
}

/// EndTurn text. Closing thoughts that the spec marks as private — not
/// delivered. Must be non-empty so the agent loop doesn't treat the turn as
/// `EmptyReply`.
fn end_turn(text: &'static str) -> ChatResponse {
    ChatResponse {
        content: vec![AssistantContent::Text(text.into())],
        stop_reason: StopReason::EndTurn,
        ..Default::default()
    }
}

#[allow(clippy::too_many_lines)] // single end-to-end scenario; splitting it would obscure the trace.
#[sqlx::test]
async fn translator_delegation_round_trips_and_emits_root_done(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let clock = SystemClock::shared();

    // ── Agents — Coordinator is the seeded default; Translator is a fresh
    // row created against the live agents store. ─────────────────────────
    let coordinator_id = seed.agent_id;
    let agent_store: SharedAgentStore = common::pg::shared_agent_store(pool.clone(), clock.clone());
    let translator_record = agent_store
        .create(NewAgent {
            org_id: seed.org_id,
            name: AgentName::try_from("translator").expect("name"),
            system_prompt: AgentSystemPrompt::try_from(
                "You translate phrases into French. Reply via send_message.",
            )
            .expect("prompt"),
            description: patom::agents::AgentDescription::try_from(
                "Translates phrases between English and other languages.",
            )
            .expect("desc"),
            is_default: false,
            allowed_mcp_tools: patom::agents::AllowedMcpTools::empty(),
            model: None,
            avatar_url: None,
            edited_by: None,
        })
        .await
        .expect("create translator");
    let translator_id = translator_record.id;

    // ── Per-agent scripts ────────────────────────────────────────────────
    // Coordinator runs twice across the conversation:
    //   1) Initial: receives the human's prompt, delegates to Translator.
    //   2) On the Translator's reply: forwards the answer to the human.
    // Translator runs once:
    //   1) Receives the request, sends the translation back to Coordinator.
    //
    // Each "run" consumes two ChatResponses: one ToolCall turn, one EndTurn.
    let coordinator_provider = AgentBoundProvider::new(
        "coordinator",
        vec![
            // Run 1: delegate.
            send_message_chunk(
                json!({ "kind": "agent", "name": "translator" }),
                "translate 'hello' to French, reply with the single word",
                "co-call-1",
            ),
            end_turn("(coordinator dispatched the request)"),
            // Run 2: forward to human.
            send_message_chunk(
                json!({ "kind": "human" }),
                "The translation is 'bonjour'.",
                "co-call-2",
            ),
            end_turn("(coordinator delivered the answer)"),
        ],
    );
    let translator_provider = AgentBoundProvider::new(
        "translator",
        vec![
            send_message_chunk(
                json!({ "kind": "agent", "name": "test-default" }),
                "bonjour",
                "tr-call-1",
            ),
            end_turn("(translator wrote back to coordinator)"),
        ],
    );

    // ── Pipeline collaborators ───────────────────────────────────────────
    let queue_impl = Arc::new(PgPromptQueue::new(pool.clone(), clock.clone()));
    let queue = queue_impl.clone();
    let leases = queue_impl.clone();
    let hub: Arc<PgResponseHub> = Arc::new(PgResponseHub::new(pool.clone(), clock.clone()));
    let sink = hub.clone();
    let sessions: SharedSessionStore = Arc::new(PgSessionStore::new(pool.clone(), clock.clone()));
    let dag: SharedDagBudget = Arc::new(PgDagBudget::new(pool.clone()));
    let memory: SharedMemory = Arc::new(StaticMemory::new("test"));
    let model = Model::try_from("test-model").expect("catalog");

    let colleagues: patom::colleagues::SharedColleagueStore =
        Arc::new(patom::colleagues::PgColleagueStore::new(pool.clone()));
    let tool_registry = ToolRegistry::builder()
        .with(Arc::new(SendMessageTool::new(
            sessions.clone(),
            queue.clone(),
            dag.clone(),
            agent_store.clone(),
            colleagues.clone(),
            sink.clone(),
        )))
        .build();
    let toolbox = ToolBox::from_builtins(tool_registry);

    // Per-agent factory: pick the right scripted provider when the registry
    // builds an Agent for a given record. CachedAgents caches the result
    // per id, so the same Arc<Agent> is handed back across claims and the
    // provider's cursor advances across the agent's two runs.
    let providers_by_id: Arc<HashMap<AgentId, Arc<AgentBoundProvider>>> =
        Arc::new(HashMap::from([
            (coordinator_id, coordinator_provider.clone()),
            (translator_id, translator_provider.clone()),
        ]));
    let factory: AgentFactory = {
        let providers_by_id = providers_by_id.clone();
        let sessions = sessions.clone();
        let memory = memory.clone();
        let toolbox = toolbox.clone();
        let clock = clock.clone();
        Arc::new(move |record| {
            let provider: SharedProvider = providers_by_id
                .get(&record.id)
                .cloned()
                .expect("test wired a provider for every seeded agent");
            // Per-agent registry: the scripted provider for this row is
            // inserted under the model's catalog provider so
            // `Agent::call_provider` routes to it.
            let providers: SharedProviderRegistry = Arc::new(
                ProviderRegistry::builder()
                    .insert(ProviderId::Anthropic, provider)
                    .build(),
            );
            AgentBuilder::new(providers, sessions.clone(), memory.clone(), model)
                .expect("builder")
                .with_clock(clock.clone())
                .with_tools(toolbox.clone())
                .with_hooks(HookChain::new())
                .build()
        })
    };
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
            patom::clock::SystemClock::shared(),
            common::embedding::FakeEmbeddingProvider::shared(),
        ));

    // Single worker — the scenario relies on serialised processing
    // (Coordinator first-run → Translator → Coordinator second-run). With
    // multiple workers the order is non-deterministic but the assertions
    // below would still hold because the agents only communicate via
    // `send_message`; a single worker just makes the trace easier to
    // reason about during failure triage.
    let cfg = WorkerConfig {
        workers: 1,
        lease_timing: LeaseTiming::try_new(Duration::from_secs(2), Duration::from_millis(100))
            .expect("valid timing"),
        max_turn_duration: Duration::from_secs(10),
        idle_poll: Duration::from_millis(20),
        cancel_poll: Duration::from_millis(50),
    };
    let workers: WorkerPoolHandle = WorkerPool::new(
        queue.clone(),
        leases,
        sink,
        agents_registry,
        sessions.clone(),
        dag.clone(),
        pool.clone(),
        memory_store_for_pool,
        clock.clone(),
        cfg,
    )
    .spawn();

    // Subscribe to the DAG-wide thread stream **before** enqueuing so we
    // don't race the worker writing the first chunks. Mirrors the production
    // FE's `/threads/{root}/stream` consumer: chunks published anywhere in
    // the DAG (root, sub-agent fan-outs, the Coordinator's second-run reply
    // to the human) fan in here, routed by `prompt_requests.root_request_id`.
    let thread_stream: SharedThreadStream =
        PgThreadStream::spawn(pool.clone(), CancellationToken::new())
            .await
            .expect("spawn thread stream");

    // ── The human kicks the conversation off ─────────────────────────────
    let outcome = queue
        .enqueue(NewPromptRequest {
            session: None,
            sender: common::pg::human_participant(&pool, seed.org_id, seed.user_id).await,
            receiver_agent_id: coordinator_id,
            parent_session: None,
            content: Prompt::try_from("translate 'hello' to French please").expect("prompt"),
            idempotency_key: IdempotencyKey::try_from("a2a-root").expect("key"),
            org_id: seed.org_id,
            created_by_user_id: seed.user_id,
            kind_payload: patom::runtime::RequestKindPayload::Normal {},
        })
        .await
        .expect("enqueue root");
    let root_id = outcome.request_id();

    // Drain the thread fan-in until the worker publishes the terminal `Done`
    // on quiescence. AgentMessage chunks (Coordinator → Human, published
    // mid-turn from the Coordinator's second run) and the terminal Done
    // both arrive here regardless of which `prompt_request` they were
    // published on — the fan-in routes by `root_request_id`.
    let chunks = drain_thread_chunks(thread_stream, root_id, Duration::from_secs(15)).await;

    // Final root status should be Done — the human's request was the DAG
    // root; it completed normally.
    let view = queue.status(root_id).await.expect("status");
    assert!(
        matches!(view.status, RequestStatus::Done),
        "expected root Done; got {:?} (failure_reason={:?})",
        view.status,
        view.failure_reason,
    );

    // Every send_message — agent→agent and agent→human alike — publishes an
    // AgentMessage on the DAG stream so observers (Slack pump, web SSE) see
    // the whole exchange, not just the final reply to the human. This DAG
    // has three sends:
    //   Coordinator → Translator   ("translate 'hello' …")
    //   Translator  → Coordinator  ("bonjour")
    //   Coordinator → Human        ("The translation is 'bonjour'.")
    let agent_messages: Vec<(&AgentId, &String)> = chunks
        .iter()
        .filter_map(|c| match c {
            ResponseChunk::AgentMessage { from, content, .. } => Some((from, content)),
            _ => None,
        })
        .collect();
    assert_eq!(
        agent_messages.len(),
        3,
        "expected three AgentMessages (two agent↔agent, one agent→human); got {} (chunks={chunks:?})",
        agent_messages.len(),
    );
    // Coordinator → Translator delegation is now visible.
    assert!(
        agent_messages.iter().any(
            |(from, content)| **from == coordinator_id && content.contains("translate 'hello'")
        ),
        "coordinator→translator handoff should surface; got {agent_messages:?}",
    );
    // Translator → Coordinator reply is now visible.
    assert!(
        agent_messages
            .iter()
            .any(|(from, content)| **from == translator_id && content.as_str() == "bonjour"),
        "translator→coordinator reply should surface; got {agent_messages:?}",
    );
    // Coordinator → Human final answer (unchanged).
    assert!(
        agent_messages
            .iter()
            .any(|(from, content)| **from == coordinator_id
                && content.as_str() == "The translation is 'bonjour'."),
        "coordinator→human answer should surface; got {agent_messages:?}",
    );

    // Both providers consumed the full script — no script entries left
    // unused (which would mean the conversation ended early) and no
    // exhaustion (which would mean a turn ran an extra time).
    assert_eq!(
        coordinator_provider.calls(),
        4,
        "Coordinator should have run twice (2 ChatResponses each)",
    );
    assert_eq!(
        translator_provider.calls(),
        2,
        "Translator should have run once",
    );

    // DAG budget bumped once per *agent*-spawning send_message — the human
    // delivery does not consume an agent turn (Stage 7), so only the two
    // agent→agent sends bump:
    //   Coordinator → Translator
    //   Translator → Coordinator
    //   (Coordinator → Human does NOT bump)
    let (turns_used, turns_cap): (i64, i64) = sqlx::query_as(
        "SELECT turns_used, turns_cap FROM prompt_request_dags WHERE root_request_id = $1",
    )
    .bind(root_id)
    .fetch_one(&pool)
    .await
    .expect("dag row");
    assert_eq!(turns_used, 2, "two agent→agent send_message bumps observed");
    assert!(turns_cap > turns_used, "well under cap (cap={turns_cap})");

    // Three sessions exist for the DAG — one per pair: (Human, Coordinator),
    // (Coordinator, Translator), and (Human, Translator) is NOT created
    // because Translator only ever messages Coordinator.
    let session_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM sessions WHERE root_request_id = $1")
            .bind(root_id)
            .fetch_one(&pool)
            .await
            .expect("session count");
    assert_eq!(
        session_count, 2,
        "expected exactly two sessions in the DAG (human↔coordinator, coordinator↔translator); got {session_count}",
    );

    // Wind the worker pool down.
    workers.shutdown().await;
}

async fn drain_thread_chunks(
    stream: SharedThreadStream,
    root: PromptRequestId,
    deadline: Duration,
) -> Vec<ResponseChunk> {
    let mut sub = stream.subscribe(root);
    let mut got = Vec::new();
    let until = std::time::Instant::now() + deadline;
    while std::time::Instant::now() < until {
        let next = tokio::time::timeout(Duration::from_millis(200), sub.next()).await;
        let Ok(Some(item)) = next else { continue };
        if let Ok(ThreadStreamEvent::Item(envelope)) = item {
            let terminal = envelope.chunk.is_terminal();
            got.push(envelope.chunk);
            if terminal {
                return got;
            }
        }
    }
    got
}
