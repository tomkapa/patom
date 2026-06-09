//! Worker-pool integration harness for the multi-agent step §13 tests.
//!
//! Boots a real Postgres-backed pipeline (queue, response hub, session
//! store, dag budget, agent registry, one worker) wired around a
//! [`ScriptedProvider`] that hands back pre-recorded `ChatResponse`s. Tests
//! then enqueue a human prompt, observe the SSE stream / queue status, and
//! assert the new multi-agent contracts (send_message round-trip, ping-pong
//! guard, quiescence-Done on root, dag-budget rejection).
//!
//! Built on the `PgPool` injected by `#[sqlx::test]` (which owns the
//! per-test database lifecycle). Worker pool is spawned with a single worker
//! for deterministic ordering during assertions; tests that need parallelism
//! can build their own pool.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use patom::agent_core::AgentBuilder;
use patom::agents::{
    AGENT_PROMPT_CACHE_CAP, AGENT_PROMPT_CACHE_TTL, AgentFactory, CachedAgents, SharedAgentStore,
    SharedAgents,
};
use patom::clock::SystemClock;
use patom::hook::HookChain;
use patom::memory::{SharedMemory, StaticMemory};
use patom::provider::{
    ChatRequest, ChatResponse, LlmProvider, Model, ProviderError, ProviderId, ProviderRegistry,
    SharedProvider, SharedProviderRegistry,
};
use patom::runtime::{
    LeaseTiming, PgDagBudget, PgPromptQueue, PgResponseHub, SharedDagBudget, WorkerConfig,
    WorkerPool, WorkerPoolHandle,
};
use patom::session::{PgSessionStore, SharedSessionStore};
use patom::tools::system::SendMessageTool;
use patom::tools::{ToolBox, ToolRegistry};
use sqlx::PgPool;

use super::pg::seed_tenant;

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

/// All the live handles the test will poke. The per-test database is owned
/// and reaped by `#[sqlx::test]`, not this struct.
pub struct WorkerHarness {
    pub queue: Arc<PgPromptQueue>,
    pub hub: Arc<PgResponseHub>,
    pub sessions: SharedSessionStore,
    pub dag: SharedDagBudget,
    pub pool: PgPool,
    pub default_agent_id: patom::agents::AgentId,
    pub default_agent_colleague_id: patom::colleagues::ColleagueId,
    /// Seeded owning org id — needed by `NewPromptRequest` and any
    /// helper that mints a fresh session under this harness's tenant.
    pub default_org_id: patom::auth::OrgId,
    /// Seeded owning user id — pairs with `default_org_id` to pin
    /// sessions created via this harness to the test principal.
    pub default_user_id: patom::auth::UserId,
    pub default_user_colleague_id: patom::colleagues::ColleagueId,
    pub workers: WorkerPoolHandle,
}

impl WorkerHarness {
    /// Build a colleague-backed `Participant` for the seeded default agent.
    pub fn default_agent_participant(&self) -> patom::types::Participant {
        patom::types::Participant::agent(self.default_agent_colleague_id, self.default_agent_id)
    }

    /// Build a colleague-backed `Participant` for the seeded human user.
    pub fn default_human_participant(&self) -> patom::types::Participant {
        patom::types::Participant::human(self.default_user_colleague_id, self.default_user_id)
    }
}

/// Build a single-worker harness with a [`SendMessageTool`] registered in
/// the agent's tool box. The `pool` is the freshly-migrated database injected
/// by `#[sqlx::test]`; the provider is the script of model responses; the
/// agent_id is the seeded default agent.
pub async fn build_harness(pool: PgPool, provider: Arc<ScriptedProvider>) -> WorkerHarness {
    let seed = seed_tenant(&pool).await;
    let clock = SystemClock::shared();

    let queue_impl = Arc::new(PgPromptQueue::new(pool.clone(), clock.clone()));
    let queue = queue_impl.clone();

    let hub = Arc::new(PgResponseHub::new(pool.clone(), clock.clone()));
    let sink = hub.clone();

    let sessions: SharedSessionStore = Arc::new(PgSessionStore::new(pool.clone(), clock.clone()));
    let threads: patom::threads::SharedThreadStore = Arc::new(patom::threads::PgThreadStore::new(
        pool.clone(),
        clock.clone(),
    ));
    let agent_store: SharedAgentStore = super::pg::shared_agent_store(pool.clone(), clock.clone());
    let colleagues: patom::colleagues::SharedColleagueStore =
        Arc::new(patom::colleagues::PgColleagueStore::new(pool.clone()));
    let dag: SharedDagBudget = Arc::new(PgDagBudget::new(pool.clone()));

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
            threads.clone(),
            queue.clone(),
            dag.clone(),
            agent_store.clone(),
            colleagues.clone(),
            sink.clone(),
        )))
        .build();
    let toolbox = ToolBox::from_builtins(tool_registry);

    let agent = AgentBuilder::new(providers, sessions.clone(), memory, model)
        .expect("builder")
        .with_clock(clock.clone())
        .with_thread_store(threads.clone())
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
        sink,
        agents_registry,
        threads.clone(),
        dag.clone(),
        cfg,
    )
    .spawn();

    let default_agent_colleague_id =
        patom::colleagues::resolve_agent_colleague(&pool, seed.org_id, seed.agent_id)
            .await
            .expect("seed agent colleague");
    let default_user_colleague_id =
        patom::colleagues::resolve_user_colleague(&pool, seed.org_id, seed.user_id)
            .await
            .expect("seed user colleague");

    WorkerHarness {
        queue,
        hub,
        sessions,
        dag,
        pool,
        default_agent_id: seed.agent_id,
        default_agent_colleague_id,
        default_org_id: seed.org_id,
        default_user_id: seed.user_id,
        default_user_colleague_id,
        workers,
    }
}
