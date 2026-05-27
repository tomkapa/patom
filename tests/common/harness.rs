//! Worker-pool integration harness for the multi-agent step §13 tests.
//!
//! Boots a real Postgres-backed pipeline (queue, response hub, session
//! store, dag budget, agent registry, one worker) wired around a
//! [`ScriptedProvider`] that hands back pre-recorded `ChatResponse`s. Tests
//! then enqueue a human prompt, observe the SSE stream / queue status, and
//! assert the new multi-agent contracts (send_message round-trip, ping-pong
//! guard, quiescence-Done on root, dag-budget rejection).
//!
//! Holds an `Arc<TestDb>` so the schema survives until the harness is
//! dropped. Worker pool is spawned with a single worker for deterministic
//! ordering during assertions; tests that need parallelism can build their
//! own pool.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use patom_rs::agent_core::AgentBuilder;
use patom_rs::agents::{
    AGENT_PROMPT_CACHE_CAP, AGENT_PROMPT_CACHE_TTL, AgentFactory, CachedAgents, SharedAgentStore,
    SharedAgents,
};
use patom_rs::clock::SystemClock;
use patom_rs::hook::HookChain;
use patom_rs::memory::{SharedMemory, StaticMemory};
use patom_rs::provider::{
    ChatRequest, ChatResponse, LlmProvider, Model, ProviderError, ProviderId, ProviderRegistry,
    SharedProvider, SharedProviderRegistry,
};
use patom_rs::runtime::{
    LeaseTiming, PgDagBudget, PgPromptQueue, PgResponseHub, SharedDagBudget, WorkerConfig,
    WorkerPool, WorkerPoolHandle,
};
use patom_rs::session::{PgSessionStore, SharedSessionStore};
use patom_rs::tools::system::SendMessageTool;
use patom_rs::tools::{ToolBox, ToolRegistry};

use super::pg::TestDb;

/// Provider that replays a fixed script of [`ChatResponse`]s — one per
/// `send` call. Tests pre-record what the model "says" each turn.
#[derive(Debug)]
pub struct ScriptedProvider {
    responses: Vec<ChatResponse>,
    cursor: AtomicUsize,
}

impl ScriptedProvider {
    pub fn new(responses: Vec<ChatResponse>) -> Self {
        Self {
            responses,
            cursor: AtomicUsize::new(0),
        }
    }

    /// How many `send` calls the harness has dispatched so far. Useful for
    /// the ping-pong test which asserts the worker called the model
    /// `MAX_PINGPONG_RETRIES + 1` times before giving up.
    pub fn calls(&self) -> usize {
        self.cursor.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl LlmProvider for ScriptedProvider {
    fn name(&self) -> &'static str {
        "scripted"
    }
    async fn send(&self, _request: ChatRequest) -> Result<ChatResponse, ProviderError> {
        let i = self.cursor.fetch_add(1, Ordering::SeqCst);
        self.responses
            .get(i)
            .cloned()
            .ok_or_else(|| ProviderError::Transport("script exhausted".into()))
    }
}

/// All the live handles the test will poke. Drop reaps the schema (via
/// `TestDb`) after the worker pool winds down.
pub struct WorkerHarness {
    pub queue: Arc<PgPromptQueue>,
    pub hub: Arc<PgResponseHub>,
    pub sessions: SharedSessionStore,
    pub dag: SharedDagBudget,
    pub default_agent_id: patom_rs::agents::AgentId,
    /// Seeded owning org id — needed by `NewPromptRequest` and any
    /// helper that mints a fresh session under this harness's tenant.
    pub default_org_id: patom_rs::auth::OrgId,
    /// Seeded owning user id — pairs with `default_org_id` to pin
    /// sessions created via this harness to the test principal.
    pub default_user_id: patom_rs::auth::UserId,
    pub workers: WorkerPoolHandle,
    /// Held only so its `Drop` reaps the schema at end-of-scope.
    #[allow(dead_code)]
    pub db: TestDb,
}

/// Build a single-worker harness with a [`SendMessageTool`] registered in
/// the agent's tool box. The provider is the script of model responses; the
/// agent_id is the seeded default agent.
pub async fn build_harness(provider: Arc<ScriptedProvider>) -> WorkerHarness {
    let db = TestDb::fresh().await;
    let clock = SystemClock::shared();

    let queue_impl = Arc::new(PgPromptQueue::new(db.pool.clone(), clock.clone()));
    let queue = queue_impl.clone();
    let leases = queue_impl.clone();

    let hub = Arc::new(PgResponseHub::new(db.pool.clone(), clock.clone()));
    let sink = hub.clone();

    let sessions: SharedSessionStore =
        Arc::new(PgSessionStore::new(db.pool.clone(), clock.clone()));
    let agent_store: SharedAgentStore =
        super::pg::shared_agent_store(db.pool.clone(), clock.clone());
    let dag: SharedDagBudget = Arc::new(PgDagBudget::new(db.pool.clone()));
    let memory_store: patom_rs::memory::SharedMemoryStore =
        Arc::new(patom_rs::memory::PgMemoryStore::new(
            db.pool.clone(),
            clock.clone(),
            super::embedding::FakeEmbeddingProvider::shared(),
        ));

    let provider: SharedProvider = provider;
    let memory: SharedMemory = Arc::new(StaticMemory::new("test"));
    let model = Model::try_from("test-model").expect("catalog");
    let providers: SharedProviderRegistry = Arc::new(
        ProviderRegistry::builder()
            .insert(ProviderId::Anthropic, provider.clone())
            .build(),
    );

    let tool_registry = ToolRegistry::builder()
        .with(Arc::new(SendMessageTool::new(
            sessions.clone(),
            queue.clone(),
            dag.clone(),
            agent_store.clone(),
            sink.clone(),
        )))
        .build();
    let toolbox = ToolBox::from_builtins(tool_registry);

    let agent = AgentBuilder::new(providers, sessions.clone(), memory, model)
        .expect("builder")
        .with_clock(clock.clone())
        .with_tools(toolbox)
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

    let cfg = WorkerConfig {
        workers: 1,
        lease_timing: LeaseTiming::try_new(Duration::from_secs(2), Duration::from_millis(100))
            .expect("valid timing"),
        max_turn_duration: Duration::from_secs(10),
        idle_poll: Duration::from_millis(20),
        cancel_poll: Duration::from_millis(50),
    };
    let workers = WorkerPool::new(
        queue.clone(),
        leases,
        sink,
        agents_registry,
        sessions.clone(),
        dag.clone(),
        db.pool.clone(),
        memory_store,
        clock.clone(),
        cfg,
    )
    .spawn();

    let default_agent_id = db.default_agent_id;
    let default_org_id = db.default_org_id;
    let default_user_id = db.default_user_id;
    WorkerHarness {
        queue,
        hub,
        sessions,
        dag,
        default_agent_id,
        default_org_id,
        default_user_id,
        workers,
        db,
    }
}
