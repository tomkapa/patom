//! Composition root.
//!
//! Wires every trait object the agent and runtime need. Each piece is constructed
//! once at startup; adding a new tool, swapping the queue backend for Postgres, or
//! chaining a policy hook is a one-line change here — the agent and runtime
//! themselves do not move.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use reqwest::Client;
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::agent_core::{Agent, AgentBuilder};
use crate::agents::{
    AGENT_PROMPT_CACHE_CAP, AGENT_PROMPT_CACHE_TTL, AgentDescription, AgentFactory, AgentName,
    AgentNamesCache, AgentPromptCache, AgentStoreError, AgentSystemPrompt, CachedAgents,
    DefaultAgentSeed, PgAgentStore, SharedAgentStore, SharedAgents,
};
use crate::auth::{
    GoogleOAuth, JwtSigner, Language, OrgId, PgOrgLanguageResolver, PgUserStore,
    SharedOrgLanguageResolver, SharedUserStore,
};
use crate::clock::{SharedClock, SystemClock};
use crate::config::{EmbeddingSettings, ProviderSettings, Settings};
use crate::crypto::OrgEncryptor;
use crate::error::AppError;
use crate::hook::HookChain;
use crate::http::{AppState, router};
use crate::mcp::oauth::{
    OAuthFlowClient, OAuthRefresher, PgMcpOAuthClientStore, PgMcpOAuthPendingStore, RefresherDeps,
    SharedMcpOAuthClientStore, SharedMcpOAuthPendingStore,
};
use crate::mcp::{
    McpRefresher, McpRegistry, PgMcpCredentialStore, PgMcpServerStore, ScopedMcpSource,
    SharedMcpCredentialStore, SharedMcpServerStore,
};
use crate::memory::{
    AgentMemory, LibrarianScheduler, MemorySectionLoader, PgMemoryStore, ReflectionScheduler,
    SESSION_MEMORY_CACHE_CAP, SESSION_MEMORY_CACHE_TTL_SECS, SessionMemoryCache, SharedMemory,
    SharedMemoryStore,
};
use crate::prompts::Prompts;
use crate::provider::anthropic::AnthropicProvider;
use crate::provider::openai::{OpenAiEmbeddingProvider, OpenAiProvider};
use crate::provider::{SharedEmbeddingProvider, SharedProvider};
use crate::runtime::{
    PgDagBudget, PgPromptQueue, PgResponseHub, PgThreadStream, SharedDagBudget, SharedLeaseManager,
    SharedPromptQueue, SharedResponseSink, SharedResponseSource, SharedThreadStream, WorkerConfig,
    WorkerPool, WorkerPoolHandle,
};
use crate::scheduling::{
    DefaultTimezone, PgScheduledTaskStore, ScheduledTaskScheduler, SharedScheduledTaskStore,
    Timezone,
};
use crate::session::{PgSessionStore, SharedSessionStore};
use crate::tools::system::{
    CancelScheduledTaskTool, CreateAgentTool, GetSessionTool, ListScheduledTasksTool,
    MemoryForgetTool, MemoryToolDeps, MemoryUpdateTool, MemoryValidateTool, MemoryWriteTool,
    RecallTool, ScheduleTaskTool, SearchAgentsTool, SendMessageTool, WebFetchTool, WebSearchTool,
};
use crate::tools::{ToolBox, ToolRegistry};

const HTTP_USER_AGENT: &str = concat!("relay-rs/", env!("CARGO_PKG_VERSION"));
const HTTP_DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);
const HTTP_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

const PG_MAX_CONNECTIONS: u32 = 32;
const PG_ACQUIRE_TIMEOUT: Duration = Duration::from_secs(5);

// Prompt bodies used to live here as `const &str`. They now live in
// `src/prompts/{internal,en,vi}.toml` and are loaded into the per-process
// [`Prompts`] registry at startup. The constants below kept the file
// growing past 300 lines and made adding a second language a structural
// edit; the registry makes "drop a sibling TOML" the entire process.

/// Name of the seeded default agent for each new personal org.
///
/// The role + description bodies for this seed live in the per-language
/// prompt registry (`src/prompts/{en,vi}.toml`); only the agent's `name`
/// is constant across languages so cross-language routing/messaging keeps
/// working regardless of the org's chosen language.
const DEFAULT_AGENT_NAME: &str = "recruiter";

// Note: every prompt body — the `<core>` family, the recruiter's role +
// description in each supported language — lives in
// `src/prompts/{internal,en,vi}.toml`, loaded once at startup into the
// [`Prompts`] registry. `default_agent_seed` reads from the registry to
// pick the right localized role + description per org.

/// All the pieces a deployment needs to serve HTTP + run workers in-process.
#[derive(Debug)]
pub struct Server {
    pub state: AppState,
    pub workers: WorkerPoolHandle,
    pub mcp_refresher: McpRefresher,
    pub oauth_refresher: OAuthRefresher,
    pub reflection_scheduler: ReflectionScheduler,
    pub librarian_scheduler: LibrarianScheduler,
    pub scheduling_scheduler: ScheduledTaskScheduler,
    pub http_addr: SocketAddr,
}

/// Pre-built collaborators shared by the agent and the runtime.
#[derive(Debug)]
struct Collaborators {
    provider: SharedProvider,
    pool: PgPool,
    sessions: SharedSessionStore,
    agents: SharedAgentStore,
    memory: SharedMemory,
    memory_store: SharedMemoryStore,
    clock: SharedClock,
    builtin_tools: ToolRegistry,
    queue: SharedPromptQueue,
    leases: SharedLeaseManager,
    dag: SharedDagBudget,
    sink: SharedResponseSink,
    responses: SharedResponseSource,
    mcp_store: SharedMcpServerStore,
    mcp_credentials: SharedMcpCredentialStore,
    mcp_oauth_clients: SharedMcpOAuthClientStore,
    mcp_oauth_pending: SharedMcpOAuthPendingStore,
    mcp_oauth_flow: OAuthFlowClient,
    mcp_encryptor: crate::crypto::SharedOrgEncryptor,
    mcp_registry: McpRegistry,
    scheduled_tasks: SharedScheduledTaskStore,
    /// Identity-table store. Built here so the per-org language resolver
    /// (which reads `organizations.default_language`) can share one
    /// `Arc<dyn UserStore>` with the OAuth callback and the `/me` routes.
    users: SharedUserStore,
    /// Per-process prompt registry — `<core>` family and per-language
    /// recruiter seed bodies.
    prompts: Arc<Prompts>,
    /// Per-agent language lookup used by `AgentMemory` to render the
    /// `<language>` tag on every turn.
    language_resolver: SharedOrgLanguageResolver,
}

impl Collaborators {
    // Composition root: a straight-line constructor that wires every
    // collaborator once. The line cap (CLAUDE.md §4) targets logic
    // functions; this one is configuration plus binding, not branching.
    #[allow(clippy::too_many_lines)]
    async fn new(settings: &Settings) -> Result<Self, AppError> {
        let http = build_http_client()?;
        let clock = SystemClock::shared();
        let pool = connect_pool(settings).await?;
        sqlx::migrate!("./migrations")
            .run(&pool)
            .await
            .map_err(|source| AppError::Migrate { source })?;

        let embedding_provider: SharedEmbeddingProvider =
            build_embedding_provider(&settings.embedding);

        // Per-org default-agent seeding happens lazily on first sign-up
        // (see `seed_default_agent_for_org` and `auth::callback`); the
        // composition root no longer mints a global default because there
        // is no global org to own it.
        let agents_impl = Arc::new(PgAgentStore::new(
            pool.clone(),
            clock.clone(),
            embedding_provider.clone(),
        ));
        let agents: SharedAgentStore = agents_impl;

        let sessions: SharedSessionStore =
            Arc::new(PgSessionStore::new(pool.clone(), clock.clone()));

        let cache = AgentPromptCache::new(
            AGENT_PROMPT_CACHE_CAP,
            AGENT_PROMPT_CACHE_TTL,
            clock.clone(),
        );
        let names_cache = AgentNamesCache::new(
            AGENT_PROMPT_CACHE_CAP,
            AGENT_PROMPT_CACHE_TTL,
            clock.clone(),
        );
        let memory_store: SharedMemoryStore = Arc::new(PgMemoryStore::new(
            pool.clone(),
            clock.clone(),
            embedding_provider.clone(),
        ));
        let session_memory_cache = SessionMemoryCache::new(
            SESSION_MEMORY_CACHE_CAP,
            Duration::from_secs(SESSION_MEMORY_CACHE_TTL_SECS),
            clock.clone(),
        );
        // One loader, two consumers — `AgentMemory` (system-prompt
        // assembly) and `MemoryToolDeps` (handle resolution inside the
        // mutation tools). Sharing the loader is what keeps the
        // contextual layer consistent across the two paths that write
        // into the same `(session, agent)` cache key.
        let memory_loader = MemorySectionLoader::new(
            memory_store.clone(),
            sessions.clone(),
            embedding_provider.clone(),
            session_memory_cache.clone(),
        );
        // Identity-side store + per-org language resolver. Built before
        // `AgentMemory` so the resolver can be cloned into it; the
        // resolver also rides on `AppState` so the PATCH
        // /me/org/language handler can invalidate after a switch.
        let users: SharedUserStore = Arc::new(PgUserStore::new(pool.clone()));
        let prompts = Arc::new(Prompts::load());
        let language_resolver: SharedOrgLanguageResolver = Arc::new(PgOrgLanguageResolver::new(
            agents.clone(),
            users.clone(),
            clock.clone(),
        ));

        let memory: SharedMemory = Arc::new(AgentMemory::new(
            agents.clone(),
            cache,
            names_cache,
            memory_loader.clone(),
            prompts.clone(),
            language_resolver.clone(),
            clock.clone(),
        ));

        let mcp_store: SharedMcpServerStore =
            Arc::new(PgMcpServerStore::new(pool.clone(), clock.clone()));
        let encryptor = Arc::new(
            OrgEncryptor::from_settings(&settings.auth.master_kek)
                .map_err(|e| AppError::Misconfigured(format!("RELAY_MASTER_KEK: {e}")))?,
        );
        let mcp_credentials: SharedMcpCredentialStore = Arc::new(PgMcpCredentialStore::new(
            pool.clone(),
            clock.clone(),
            encryptor.clone(),
        ));
        let mcp_oauth_clients: SharedMcpOAuthClientStore = Arc::new(PgMcpOAuthClientStore::new(
            pool.clone(),
            clock.clone(),
            encryptor.clone(),
        ));
        let mcp_oauth_pending: SharedMcpOAuthPendingStore =
            Arc::new(PgMcpOAuthPendingStore::new(pool.clone(), clock.clone()));
        let mcp_oauth_flow = OAuthFlowClient::new(http.clone())
            .map_err(|e| AppError::Misconfigured(format!("mcp oauth flow http: {e}")))?;
        let mcp_registry = McpRegistry::with_credentials(
            mcp_store.clone(),
            Some(mcp_credentials.clone()),
            clock.clone(),
        );

        let scheduled_tasks: SharedScheduledTaskStore =
            Arc::new(PgScheduledTaskStore::new(pool.clone(), clock.clone()));
        let default_tz =
            DefaultTimezone::from_timezone(Timezone::from_tz(settings.default_timezone));

        // Queue, DAG budget, and response hub are built here so the
        // `send_message` system tool can hold them without a later
        // round-trip. The hub's publish/subscribe halves split between
        // `send_message` (human-receiver branch) and the SSE route.
        let queue_impl = Arc::new(PgPromptQueue::new(pool.clone(), clock.clone()));
        let queue: SharedPromptQueue = queue_impl.clone();
        let leases: SharedLeaseManager = queue_impl;
        let dag: SharedDagBudget = Arc::new(PgDagBudget::new(pool.clone()));

        let hub = Arc::new(PgResponseHub::new(pool.clone(), clock.clone()));
        let sink: SharedResponseSink = hub.clone();
        let responses: SharedResponseSource = hub;

        // Built-in tool registration. Each tool's constructor is
        // straightforward; keeping the registration in the composition
        // root avoids a register-helper that just ferries a deps
        // struct in from this same call site. Adding a tool is one new
        // file in `tools/system/` + one `.with(...)` line here.
        // The memory tool family shares one set of deps. `recall` is the
        // only memory tool that talks to the embedding provider directly;
        // the mutation tools embed through `MemoryStore`. `memory_update`
        // and `memory_forget` close the active contradiction inline when
        // the worker is dispatching a resolution turn (via
        // `ToolCallContext::resolution_target`); the no-action close runs
        // post-turn in the worker.
        let memory_tools = MemoryToolDeps::new(memory_loader);
        let builtin_tools = build_builtin_tools(BuiltinToolDeps {
            http,
            settings,
            sessions: sessions.clone(),
            queue: queue.clone(),
            dag: dag.clone(),
            agents: agents.clone(),
            sink: sink.clone(),
            memory_tools,
            embedding_provider,
            scheduled_tasks: scheduled_tasks.clone(),
            default_tz,
            clock: clock.clone(),
            pool: pool.clone(),
        })?;

        // `memory_store` and `session_memory_cache` are not held on
        // `Collaborators`: they're cheap-clone handles already
        // distributed to every consumer (AgentMemory and the memory
        // tools) by the clones above. The reflection scheduler builds
        // its own handles from `pieces.pool` / `pieces.queue` later.
        Ok(Self {
            provider: build_provider(settings)?,
            pool,
            sessions,
            agents,
            memory,
            memory_store,
            clock,
            builtin_tools,
            queue,
            leases,
            dag,
            sink,
            responses,
            mcp_store,
            mcp_credentials,
            mcp_oauth_clients,
            mcp_oauth_pending,
            mcp_oauth_flow,
            mcp_encryptor: encryptor,
            mcp_registry,
            scheduled_tasks,
            users,
            prompts,
            language_resolver,
        })
    }
}

/// Just enough of the collaborator graph to assemble one `Agent` for one
/// `AgentRecord`. Cheap to clone (every field is either `Arc`-wrapped or
/// already `Clone` over cheap state) so the factory closure can hold it.
#[derive(Clone)]
struct AgentFactoryPieces {
    provider: SharedProvider,
    sessions: SharedSessionStore,
    memory: SharedMemory,
    clock: SharedClock,
    builtin_tools: ToolRegistry,
    mcp_registry: McpRegistry,
    model: crate::types::ModelId,
    tool_call_store: crate::tools::SharedToolCallStore,
}

impl AgentFactoryPieces {
    fn build(&self, record: &crate::agents::AgentRecord) -> Agent {
        let dynamic = Arc::new(ScopedMcpSource::new(
            self.mcp_registry.clone(),
            &record.allowed_mcp_tools,
        ));
        let toolbox = ToolBox::new(self.builtin_tools.clone(), dynamic);
        AgentBuilder::new(
            self.provider.clone(),
            self.sessions.clone(),
            self.memory.clone(),
            self.model.clone(),
        )
        .expect("invariant: limits constants are static and parse")
        .with_tools(toolbox)
        .with_hooks(HookChain::new())
        .with_clock(self.clock.clone())
        .with_tool_call_store(self.tool_call_store.clone())
        .build()
    }
}

/// Aggregated handles passed to [`build_builtin_tools`]. Keeps the
/// `Collaborators::new` body under the §4 line cap by lifting tool
/// registration into its own function.
struct BuiltinToolDeps<'a> {
    http: Client,
    settings: &'a Settings,
    sessions: SharedSessionStore,
    queue: SharedPromptQueue,
    dag: SharedDagBudget,
    agents: SharedAgentStore,
    sink: SharedResponseSink,
    memory_tools: MemoryToolDeps,
    embedding_provider: SharedEmbeddingProvider,
    scheduled_tasks: SharedScheduledTaskStore,
    default_tz: DefaultTimezone,
    clock: SharedClock,
    /// Pool handle threaded through to the scheduling tools so they can
    /// open `begin_as_user` tx for tenant-side visibility gating.
    pool: PgPool,
}

/// Register every system tool into a [`ToolRegistry`]. Lives at the
/// composition root so adding a tool is one new file in `tools/system/`
/// + one `.with(...)` line here.
fn build_builtin_tools(deps: BuiltinToolDeps<'_>) -> Result<ToolRegistry, AppError> {
    Ok(ToolRegistry::builder()
        .with(Arc::new(WebFetchTool::new()?))
        .with(Arc::new(WebSearchTool::new(
            deps.http,
            deps.settings.brave_search_api_key.clone(),
        )))
        .with(Arc::new(SendMessageTool::new(
            deps.sessions.clone(),
            deps.queue.clone(),
            deps.dag.clone(),
            deps.agents.clone(),
            deps.sink.clone(),
        )))
        .with(Arc::new(GetSessionTool::new(deps.sessions.clone())))
        .with(Arc::new(MemoryWriteTool::new(deps.memory_tools.clone())))
        .with(Arc::new(MemoryUpdateTool::new(deps.memory_tools.clone())))
        .with(Arc::new(MemoryForgetTool::new(deps.memory_tools.clone())))
        .with(Arc::new(MemoryValidateTool::new(deps.memory_tools.clone())))
        .with(Arc::new(RecallTool::new(
            deps.memory_tools,
            deps.embedding_provider.clone(),
        )))
        .with(Arc::new(SearchAgentsTool::new(
            deps.agents.clone(),
            deps.embedding_provider,
        )))
        .with(Arc::new(CreateAgentTool::new(deps.agents.clone())))
        .with(Arc::new(ScheduleTaskTool::new(
            deps.scheduled_tasks.clone(),
            deps.agents.clone(),
            deps.sessions.clone(),
            deps.default_tz,
            deps.clock,
        )))
        .with(Arc::new(ListScheduledTasksTool::new(
            deps.scheduled_tasks.clone(),
            deps.sessions.clone(),
            deps.pool.clone(),
        )))
        .with(Arc::new(CancelScheduledTaskTool::new(
            deps.scheduled_tasks,
            deps.sessions.clone(),
            deps.pool.clone(),
        )))
        .build())
}

/// Seed material for the per-org default agent in `language`.
///
/// Exposed so the OAuth callback (`auth::callback`) can mint the default
/// agent inside the just-created personal org. The role + description
/// bodies are pulled from the per-language [`Prompts`] registry — the
/// `recruiter` an `org_id` with `default_language='vi'` ends up with is
/// the Vietnamese-translated seed. Fails only if the registry bodies
/// suddenly violate a newtype invariant (a startup-time guarantee in
/// practice since `Prompts::load` panics on malformed input).
pub fn default_agent_seed(
    prompts: &Prompts,
    language: Language,
) -> Result<DefaultAgentSeed, AppError> {
    let set = prompts.set(language);
    Ok(DefaultAgentSeed {
        name: AgentName::try_from(DEFAULT_AGENT_NAME)?,
        system_prompt: AgentSystemPrompt::try_from(set.default_agent_role.as_ref())?,
        description: AgentDescription::try_from(set.default_agent_description.as_ref())?,
    })
}

/// Seed the default agent for `org_id`.
///
/// Idempotent — a second call for the same org returns the existing
/// default's id. Called from the OAuth callback on first sign-up so the
/// cookie minted immediately resolves to a usable workspace.
pub async fn seed_default_agent_for_org(
    agents: &SharedAgentStore,
    org_id: OrgId,
    seed: DefaultAgentSeed,
) -> Result<crate::agents::AgentId, AgentStoreError> {
    agents.seed_default(org_id, seed).await
}

/// Build a fully-wired [`Agent`] without the HTTP/worker stack.
///
/// Skips per-agent MCP scoping (no `AgentRecord` to scope against here) —
/// callers must not use this for production turn dispatch, which goes
/// through `build_server`'s per-agent factory below.
pub async fn build_agent(settings: Settings) -> Result<Agent, AppError> {
    let pieces = Collaborators::new(&settings).await?;
    Ok(build_agent_from(&pieces, &settings))
}

fn build_agent_from(pieces: &Collaborators, settings: &Settings) -> Agent {
    let toolbox = ToolBox::new(
        pieces.builtin_tools.clone(),
        pieces.mcp_registry.as_dynamic_source(),
    );
    let tool_call_store: crate::tools::SharedToolCallStore = Arc::new(
        crate::tools::PgToolCallStore::new(pieces.pool.clone(), pieces.clock.clone()),
    );
    AgentBuilder::new(
        pieces.provider.clone(),
        pieces.sessions.clone(),
        pieces.memory.clone(),
        settings.model.clone(),
    )
    .expect("invariant: limits constants are static and parse")
    .with_tools(toolbox)
    .with_hooks(HookChain::new())
    .with_clock(pieces.clock.clone())
    .with_tool_call_store(tool_call_store)
    .build()
}

/// Build the full HTTP + worker pool composition. The returned [`Server`] is ready to
/// hand to `axum::serve` and a graceful-shutdown loop.
#[allow(clippy::too_many_lines)] // composition root: configuration + binding, not branching
pub async fn build_server(
    settings: Settings,
    cancel: CancellationToken,
) -> Result<Server, AppError> {
    let pieces = Collaborators::new(&settings).await?;
    // Per-agent MCP scope: the factory reads the row's `allowed_mcp_servers`
    // and builds a `ScopedMcpSource` so the agent's `ToolBox` only sees the
    // permitted servers' tools. Everything else (provider, sessions, memory,
    // builtins, hooks) is cheap-clone shared across agents.
    let tool_call_store: crate::tools::SharedToolCallStore = Arc::new(
        crate::tools::PgToolCallStore::new(pieces.pool.clone(), pieces.clock.clone()),
    );
    let factory_pieces = AgentFactoryPieces {
        provider: pieces.provider.clone(),
        sessions: pieces.sessions.clone(),
        memory: pieces.memory.clone(),
        clock: pieces.clock.clone(),
        builtin_tools: pieces.builtin_tools.clone(),
        mcp_registry: pieces.mcp_registry.clone(),
        model: settings.model.clone(),
        tool_call_store,
    };
    let factory: AgentFactory = Arc::new(move |record| factory_pieces.build(record));
    let agents_registry: SharedAgents = Arc::new(CachedAgents::new(
        pieces.agents.clone(),
        factory,
        AGENT_PROMPT_CACHE_CAP,
        AGENT_PROMPT_CACHE_TTL,
        pieces.clock.clone(),
    ));

    // Best-effort initial refresh — failure logged via `last_error` and
    // the warn logs inside `refresh`; doesn't block startup.
    if let Err(e) = pieces.mcp_registry.refresh().await {
        warn!(error = %e, "mcp.refresh.startup_failed");
    }

    let (mcp_refresher, mcp_refresh) = McpRefresher::spawn(pieces.mcp_registry.clone());

    let pool = WorkerPool::new(
        pieces.queue.clone(),
        pieces.leases.clone(),
        pieces.sink.clone(),
        agents_registry,
        pieces.sessions.clone(),
        pieces.dag.clone(),
        pieces.pool.clone(),
        pieces.memory_store.clone(),
        pieces.clock.clone(),
        WorkerConfig::default(),
    );
    let workers = pool.spawn();

    // Background scheduler that periodically enqueues reflection turns
    // (doc/memory.md §1.6). The reflection itself runs through the worker
    // pool above.
    let reflection_scheduler = ReflectionScheduler::spawn(
        pieces.pool.clone(),
        pieces.queue.clone(),
        pieces.clock.clone(),
        cancel.clone(),
    );

    // Librarian — mechanical sweep per agent, plus resolution-turn
    // enqueue for unresolved contradictions (doc/memory.md §1.8).
    let librarian_scheduler = LibrarianScheduler::spawn(
        pieces.pool.clone(),
        pieces.memory_store.clone(),
        pieces.queue.clone(),
        pieces.clock.clone(),
        cancel.clone(),
    );

    // Scheduling — agent-driven scheduled tasks. Polls the
    // `scheduled_tasks` table on a fixed cadence and enqueues a
    // `prompt_requests` row for each due fire.
    let scheduling_scheduler = ScheduledTaskScheduler::spawn(
        pieces.scheduled_tasks.clone(),
        pieces.queue.clone(),
        pieces.clock.clone(),
        cancel.clone(),
    );

    // Single-process fan-in subscriber for the chat-UI thread stream. Owns
    // its own LISTEN connection on the shared pool; tied to the same cancel
    // token so a top-level shutdown winds the listener task down with the
    // rest of the runtime.
    let thread_stream: SharedThreadStream =
        PgThreadStream::spawn(pieces.pool.clone(), CancellationToken::new())
            .await
            .map_err(|source| AppError::DbConnect { source })?;

    let jwt =
        JwtSigner::new(&settings.auth.jwt_secret, pieces.clock.clone()).map_err(AppError::Auth)?;
    let oauth = GoogleOAuth::new(
        &settings.auth.google_client_id,
        &settings.auth.google_client_secret,
        &settings.auth.google_redirect_url,
    )
    .map_err(AppError::Auth)?;

    let memberships = Arc::new(crate::http::MembershipCache::new(pieces.clock.clone()));
    let mcp_test_rate = crate::mcp::TestConnectRateLimiter::new(pieces.clock.clone());

    let oauth_redirect_uri = format!(
        "{}{}",
        settings.auth.oauth_redirect_base, "/mcp-oauth/callback"
    );
    let (oauth_refresher, _oauth_token_cache) = OAuthRefresher::spawn(RefresherDeps {
        pool: pieces.pool.clone(),
        clock: pieces.clock.clone(),
        enc: pieces.mcp_encryptor.clone(),
        credentials: pieces.mcp_credentials.clone(),
        oauth_clients: pieces.mcp_oauth_clients.clone(),
        flow: pieces.mcp_oauth_flow.clone(),
        redirect_uri: oauth_redirect_uri,
    });

    let state = AppState {
        queue: pieces.queue,
        leases: pieces.leases,
        responses: pieces.responses,
        sessions: pieces.sessions,
        agents: pieces.agents,
        dag: pieces.dag,
        memory_store: pieces.memory_store.clone(),
        mcp_store: pieces.mcp_store,
        mcp_credentials: pieces.mcp_credentials,
        mcp_refresh,
        mcp_test_rate,
        mcp_oauth_clients: pieces.mcp_oauth_clients,
        mcp_oauth_pending: pieces.mcp_oauth_pending,
        mcp_oauth_flow: pieces.mcp_oauth_flow,
        oauth_redirect_base: Arc::from(settings.auth.oauth_redirect_base.as_str()),
        web_base_url: settings.auth.web_base_url.as_deref().map(Arc::from),
        thread_stream,
        pool: pieces.pool.clone(),
        jwt,
        oauth,
        users: pieces.users,
        clock: pieces.clock.clone(),
        cookie_secure: settings.auth.cookie_secure,
        memberships,
        prompts: pieces.prompts,
        language_resolver: pieces.language_resolver,
        web_dist: settings.web_dist.clone(),
    };

    Ok(Server {
        state,
        workers,
        mcp_refresher,
        oauth_refresher,
        reflection_scheduler,
        librarian_scheduler,
        scheduling_scheduler,
        http_addr: settings.http_addr,
    })
}

/// Run the server until `cancel` fires. Performs graceful shutdown of HTTP first, then
/// the worker pool — workers continue processing in-flight turns up to their per-turn
/// timeout before exiting.
pub async fn run_server(server: Server, cancel: CancellationToken) -> Result<(), AppError> {
    let Server {
        state,
        workers,
        mcp_refresher,
        oauth_refresher,
        reflection_scheduler,
        librarian_scheduler,
        scheduling_scheduler,
        http_addr,
    } = server;
    let app = router(state);
    let listener = tokio::net::TcpListener::bind(http_addr)
        .await
        .map_err(|source| AppError::Bind { http_addr, source })?;
    info!(http_addr = %http_addr, "http.listening");

    let shutdown = cancel.clone();
    let serve = axum::serve(listener, app).with_graceful_shutdown(async move {
        shutdown.cancelled().await;
    });

    if let Err(e) = serve.await {
        warn!(error = %e, "http.serve.error");
    }
    info!("http.shutdown.complete");
    // HTTP first, then schedulers (no new enqueues), then workers.
    reflection_scheduler.shutdown().await;
    info!("reflection_scheduler.shutdown.complete");
    librarian_scheduler.shutdown().await;
    info!("librarian_scheduler.shutdown.complete");
    scheduling_scheduler.shutdown().await;
    info!("scheduling_scheduler.shutdown.complete");
    mcp_refresher.shutdown().await;
    oauth_refresher.shutdown().await;
    info!("mcp.refresher.shutdown.complete");
    workers.shutdown().await;
    info!("workers.shutdown.complete");
    Ok(())
}

fn build_embedding_provider(s: &EmbeddingSettings) -> SharedEmbeddingProvider {
    Arc::new(OpenAiEmbeddingProvider::new(
        &s.api_key,
        s.base_url.clone(),
        s.model.clone(),
        s.dimensions,
    ))
}

fn build_provider(settings: &Settings) -> Result<SharedProvider, AppError> {
    let provider: SharedProvider = match &settings.provider {
        ProviderSettings::Openai { api_key, base_url } => {
            Arc::new(OpenAiProvider::new(api_key, base_url.clone()))
        }
        ProviderSettings::Anthropic { api_key, base_url } => {
            Arc::new(AnthropicProvider::new(api_key, base_url.clone())?)
        }
    };
    Ok(provider)
}

fn build_http_client() -> Result<Client, reqwest::Error> {
    Client::builder()
        .timeout(HTTP_DEFAULT_TIMEOUT)
        .connect_timeout(HTTP_CONNECT_TIMEOUT)
        .user_agent(HTTP_USER_AGENT)
        .build()
}

async fn connect_pool(settings: &Settings) -> Result<PgPool, AppError> {
    PgPoolOptions::new()
        .max_connections(PG_MAX_CONNECTIONS)
        .acquire_timeout(PG_ACQUIRE_TIMEOUT)
        .connect(settings.database_url.expose())
        .await
        .map_err(|source| AppError::DbConnect { source })
}
