//! Composition root.
//!
//! Wires every trait object the agent and runtime need. Each piece is constructed
//! once at startup; adding a new tool, swapping the queue backend for Postgres, or
//! chaining a policy hook is a one-line change here — the agent and runtime
//! themselves do not move.

use std::collections::HashMap;
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
    AgentPromptCache, AgentSeed, AgentStoreError, AgentSystemPrompt, CachedAgents, PgAgentStore,
    SharedAgentStore, SharedAgents,
};
use crate::assets::{S3AssetStore, SharedAssetStore};
use crate::auth::{
    JwtSigner, Language, OidcProvider, OrgId, PgOrgLanguageResolver, PgOrgRuleResolver,
    PgUserStore, SharedOidcAuth, SharedOrgLanguageResolver, SharedOrgRuleResolver, SharedUserStore,
};
use crate::clock::{SharedClock, SystemClock};
use crate::config::{EmbeddingSettings, Settings};
use crate::crypto::OrgEncryptor;
use crate::error::AppError;
use crate::hook::HookChain;
use crate::http::{AppState, router};
use crate::mcp::oauth::{PgMcpOAuthPendingStore, SharedMcpOAuthPendingStore};
use crate::mcp::{
    McpRefresher, McpRegistry, PgMcpCatalogStore, PgMcpCredentialStore, PgMcpServerStore,
    ScopedMcpSource, SharedMcpCatalogStore, SharedMcpCredentialStore, SharedMcpServerStore,
};
use crate::memory::{
    AgentMemory, LibrarianScheduler, MemorySectionLoader, PgMemoryStore, ReflectionScheduler,
    SharedMemory, SharedMemoryStore,
};
use crate::prompts::Prompts;
use crate::provider::anthropic::AnthropicProvider;
use crate::provider::openai::{OpenAiEmbeddingProvider, OpenAiProvider};
use crate::provider::{SharedEmbeddingProvider, SharedProviderRegistry};
use crate::runtime::{
    PgDagBudget, PgPromptQueue, PgResponseHub, PgThreadStream, SharedDagBudget, SharedPromptQueue,
    SharedResponseSink, SharedResponseSource, SharedThreadStream, WorkerConfig, WorkerPool,
    WorkerPoolHandle,
};
use crate::scheduling::{
    DefaultTimezone, PgScheduledTaskStore, ScheduledTaskScheduler, SharedScheduledTaskStore,
    Timezone,
};
use crate::tools::system::{
    CancelScheduledTaskTool, CreateAgentTool, ListScheduledTasksTool, MemoryForgetTool,
    MemoryToolDeps, MemoryUpdateTool, MemoryValidateTool, MemoryWriteTool, PgSessionTodoStore,
    RecallTool, RequestUserWireMcpTool, ScheduleTaskTool, SearchAgentsTool, SearchToolsTool,
    SendMessageTool, SharedSessionTodoStore, TodoToolDeps, TodoWriteTool, WebFetchTool,
    WebSearchTool,
};
use crate::tools::{ToolBox, ToolRegistry};

const HTTP_USER_AGENT: &str = concat!("patom/", env!("CARGO_PKG_VERSION"));
const HTTP_DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);
const HTTP_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

const PG_MAX_CONNECTIONS: u32 = 32;
const PG_ACQUIRE_TIMEOUT: Duration = Duration::from_secs(5);

// Prompt bodies used to live here as `const &str`. They now live in
// `src/prompts/{internal,en,vi}.toml` and are loaded into the per-process
// [`Prompts`] registry at startup. The constants below kept the file
// growing past 300 lines and made adding a second language a structural
// edit; the registry makes "drop a sibling TOML" the entire process.

/// Name of the preset recruiter agent seeded into each new org.
///
/// The role + description bodies for this seed live in the per-language
/// prompt registry (`src/prompts/{en,vi}.toml`); only the agent's `name`
/// is constant across languages so cross-language routing/messaging keeps
/// working regardless of the org's chosen language. Public because the
/// Slack bridge routes an un-@-tagged `@Patom` mention to this preset
/// (there is no "default agent" at runtime any more).
pub const RECRUITER_AGENT_NAME: &str = "recruiter";

// Note: every prompt body — the `<core>` family, the recruiter's role +
// description in each supported language — lives in
// `src/prompts/{internal,en,vi}.toml`, loaded once at startup into the
// [`Prompts`] registry. `recruiter_seed` reads from the registry to
// pick the right localized role + description per org.

/// All the pieces a deployment needs to serve HTTP + run workers in-process.
#[derive(Debug)]
pub struct Server {
    pub state: AppState,
    pub workers: WorkerPoolHandle,
    pub mcp_refresher: McpRefresher,
    /// Owning coordinator task for the BYO provider-overlay refresh (#141).
    /// `run_server` joins on its `shutdown()` after HTTP exits.
    pub provider_refresher: crate::provider::ProviderRefresher,
    pub reflection_scheduler: ReflectionScheduler,
    pub librarian_scheduler: LibrarianScheduler,
    pub scheduling_scheduler: ScheduledTaskScheduler,
    pub http_addr: SocketAddr,
    /// Optional Slack bridge worker — `Some` iff `settings.slack` is
    /// `Some`. `run_server` joins on `shutdown()` after HTTP exits.
    pub slack_bridge: Option<crate::slack::bridge::BridgeHandle>,
}

/// Pre-built collaborators shared by the agent and the runtime.
#[derive(Debug)]
struct Collaborators {
    providers: SharedProviderRegistry,
    pool: PgPool,
    threads: crate::threads::SharedThreadStore,
    background: crate::background::SharedBackgroundStore,
    agents: SharedAgentStore,
    colleagues: crate::colleagues::SharedColleagueStore,
    memory: SharedMemory,
    memory_store: SharedMemoryStore,
    clock: SharedClock,
    builtin_tools: ToolRegistry,
    queue: SharedPromptQueue,
    dag: SharedDagBudget,
    sink: SharedResponseSink,
    responses: SharedResponseSource,
    mcp_store: SharedMcpServerStore,
    mcp_catalog: SharedMcpCatalogStore,
    mcp_credentials: SharedMcpCredentialStore,
    mcp_oauth_pending: SharedMcpOAuthPendingStore,
    /// Env-keyed Patom-supported OAuth clients. Built once at boot and
    /// shared by both the registry's OAuth adapter and the HTTP routes
    /// that initiate / complete the OAuth flow.
    platform_oauth_clients: Arc<HashMap<String, crate::config::PlatformOAuthClient>>,
    mcp_encryptor: crate::crypto::SharedOrgEncryptor,
    mcp_registry: McpRegistry,
    /// BYO provider-credential store + per-org overlay (#141). The store backs
    /// the HTTP CRUD/validate routes; the overlay is the synchronous per-turn
    /// read the agent factory routes through. Both reuse `mcp_encryptor`.
    provider_credentials: crate::provider::SharedOrgProviderCredentialStore,
    provider_overlay: crate::provider::OrgProviderOverlay,
    scheduled_tasks: SharedScheduledTaskStore,
    /// Per-session todo store. Held here so `AgentFactoryPieces` can
    /// thread it into every spawned `Agent` (the per-turn context
    /// builder reads it to fold the current list into the system
    /// prompt).
    todos_store: SharedSessionTodoStore,
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
    /// Per-agent organization-rule lookup used by `AgentMemory` to
    /// render the `<organization-rule>` tag on every turn.
    rule_resolver: SharedOrgRuleResolver,
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
        // (see `seed_recruiter_for_org` and `auth::callback`); the
        // composition root no longer mints a global default because there
        // is no global org to own it.
        let agents_impl = Arc::new(PgAgentStore::new(
            pool.clone(),
            clock.clone(),
            embedding_provider.clone(),
        ));
        let agents: SharedAgentStore = agents_impl;

        let threads: crate::threads::SharedThreadStore = Arc::new(
            crate::threads::PgThreadStore::new(pool.clone(), clock.clone()),
        );

        let background: crate::background::SharedBackgroundStore = Arc::new(
            crate::background::PgBackgroundStore::new(pool.clone(), clock.clone()),
        );

        let colleagues: crate::colleagues::SharedColleagueStore =
            Arc::new(crate::colleagues::PgColleagueStore::new(pool.clone()));

        let cache = AgentPromptCache::new(
            AGENT_PROMPT_CACHE_CAP,
            AGENT_PROMPT_CACHE_TTL,
            clock.clone(),
        );
        let roster_cache = crate::colleagues::ColleagueRosterCache::new(
            crate::colleagues::COLLEAGUE_ROSTER_CACHE_CAP,
            crate::colleagues::COLLEAGUE_ROSTER_CACHE_TTL,
            clock.clone(),
        );
        let memory_store: SharedMemoryStore = Arc::new(PgMemoryStore::new(
            pool.clone(),
            clock.clone(),
            embedding_provider.clone(),
        ));
        // One loader, two consumers — `AgentMemory` (system-prompt
        // assembly) and `MemoryToolDeps` (handle resolution inside the
        // mutation tools). Sharing the loader keeps both paths composing the
        // same stable section, so resolved `M-NN` handles match what was
        // rendered.
        let memory_loader = MemorySectionLoader::new(
            memory_store.clone(),
            colleagues.clone(),
            roster_cache.clone(),
            embedding_provider.clone(),
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
        // Per-org rule resolver — same lifecycle as `language_resolver`;
        // cloned into `AgentMemory` (read on every turn) and threaded
        // onto `AppState` so the PATCH /me/org/rule handler can call
        // `invalidate_all` after a write.
        let rule_resolver: SharedOrgRuleResolver = Arc::new(PgOrgRuleResolver::new(
            agents.clone(),
            users.clone(),
            clock.clone(),
        ));

        let memory: SharedMemory = Arc::new(AgentMemory::new(
            agents.clone(),
            cache,
            colleagues.clone(),
            roster_cache,
            memory_loader.clone(),
            prompts.clone(),
            language_resolver.clone(),
            rule_resolver.clone(),
            clock.clone(),
        ));

        let mcp_store: SharedMcpServerStore =
            Arc::new(PgMcpServerStore::new(pool.clone(), clock.clone()));
        // Build the encryptor before the catalog store: a user-supplied OAuth
        // client's secret is sealed on the org-scoped `mcp_catalog` row, so the
        // store needs the shared encryptor (mirrors `PgMcpCredentialStore`).
        let encryptor = Arc::new(
            OrgEncryptor::from_settings(&settings.auth.master_kek)
                .map_err(|e| AppError::Misconfigured(format!("PATOM_MASTER_KEK: {e}")))?,
        );
        let mcp_catalog: SharedMcpCatalogStore =
            Arc::new(PgMcpCatalogStore::new(pool.clone(), encryptor.clone()));
        let mcp_credentials: SharedMcpCredentialStore = Arc::new(PgMcpCredentialStore::new(
            pool.clone(),
            clock.clone(),
            encryptor.clone(),
        ));
        let mcp_oauth_pending: SharedMcpOAuthPendingStore =
            Arc::new(PgMcpOAuthPendingStore::new(pool.clone(), clock.clone()));
        let platform_oauth_clients = Arc::new(settings.auth.platform_oauth_clients.clone());
        let mcp_registry = McpRegistry::with_oauth_deps(
            mcp_store.clone(),
            mcp_credentials.clone(),
            crate::mcp::OAuthAdapterDeps {
                credentials: mcp_credentials.clone(),
                catalog: mcp_catalog.clone(),
                platform_clients: platform_oauth_clients.clone(),
                inner_http: http.clone(),
            },
            clock.clone(),
        );

        // BYO provider credentials (#141): same envelope encryptor as MCP. The
        // overlay starts empty; `build_server` runs the first refresh before the
        // worker pool begins so the first turn already sees keyed providers.
        let provider_credentials: crate::provider::SharedOrgProviderCredentialStore =
            Arc::new(crate::provider::PgOrgProviderCredentialStore::new(
                pool.clone(),
                clock.clone(),
                encryptor.clone(),
            ));
        let provider_overlay =
            crate::provider::OrgProviderOverlay::new(provider_credentials.clone());

        let scheduled_tasks: SharedScheduledTaskStore =
            Arc::new(PgScheduledTaskStore::new(pool.clone(), clock.clone()));
        let default_tz =
            DefaultTimezone::from_timezone(Timezone::from_tz(settings.default_timezone));

        // Queue, DAG budget, and response hub are built here so the
        // `send_message` system tool can hold them without a later
        // round-trip. The hub's publish/subscribe halves split between
        // `send_message` (human-receiver branch) and the SSE route.
        let queue: SharedPromptQueue = Arc::new(PgPromptQueue::new(pool.clone(), clock.clone()));
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
        // `todos_store` is held on `Collaborators` so the per-turn
        // context builder can fold the current list into the system
        // prompt (the load-bearing piece for "persists across re-runs"
        // — the row exists in PG regardless, but the model only sees
        // it through this injection).
        let todos_store: SharedSessionTodoStore =
            Arc::new(PgSessionTodoStore::new(pool.clone(), clock.clone()));
        let todo_tools = TodoToolDeps::new(todos_store.clone());
        let builtin_tools = build_builtin_tools(BuiltinToolDeps {
            http,
            settings,
            threads: threads.clone(),
            queue: queue.clone(),
            dag: dag.clone(),
            agents: agents.clone(),
            colleagues: colleagues.clone(),
            sink: sink.clone(),
            memory_tools,
            todo_tools,
            embedding_provider,
            scheduled_tasks: scheduled_tasks.clone(),
            default_tz,
            clock: clock.clone(),
            pool: pool.clone(),
            mcp_catalog: mcp_catalog.clone(),
            mcp_store: mcp_store.clone(),
        })?;

        // `memory_store` is not held on `Collaborators`: it's a cheap-clone
        // handle already distributed to every consumer (AgentMemory and the
        // memory tools) by the clones above. The reflection scheduler builds
        // its own handles from `pieces.pool` / `pieces.queue` later.
        Ok(Self {
            providers: build_provider_registry(settings)?,
            pool,
            threads,
            background,
            agents,
            colleagues,
            memory,
            memory_store,
            clock,
            builtin_tools,
            queue,
            dag,
            sink,
            responses,
            mcp_store,
            mcp_catalog,
            mcp_credentials,
            mcp_oauth_pending,
            platform_oauth_clients,
            mcp_encryptor: encryptor,
            mcp_registry,
            provider_credentials,
            provider_overlay,
            scheduled_tasks,
            todos_store,
            users,
            prompts,
            language_resolver,
            rule_resolver,
        })
    }
}

/// Just enough of the collaborator graph to assemble one `Agent` for one
/// `AgentRecord`. Cheap to clone (every field is either `Arc`-wrapped or
/// already `Clone` over cheap state) so the factory closure can hold it.
#[derive(Clone)]
struct AgentFactoryPieces {
    providers: SharedProviderRegistry,
    threads: crate::threads::SharedThreadStore,
    background: crate::background::SharedBackgroundStore,
    memory: SharedMemory,
    clock: SharedClock,
    builtin_tools: ToolRegistry,
    mcp_registry: McpRegistry,
    model_resolver: crate::agents::SharedModelResolver,
    /// Per-org BYO provider overlay (#141). Threaded into every spawned
    /// `Agent` so its turns route to the org's BYO client when one is keyed,
    /// and consulted here for org-key-aware model resolution.
    provider_overlay: crate::provider::OrgProviderOverlay,
    tool_call_store: crate::tools::SharedToolCallStore,
    todos_store: SharedSessionTodoStore,
    turn_metrics_store: crate::agent_core::turn_metrics::SharedTurnMetricsStore,
    billing: crate::billing::SharedBillingService,
}

impl AgentFactoryPieces {
    fn build(&self, record: &crate::agents::AgentRecord) -> Agent {
        // Catalog → server resolution is computed once per refresh and
        // held inside the registry; this returns an `Arc<HashMap<…>>`
        // pointer clone, no DB round-trip on the per-session hot path.
        let catalog_to_server = self.mcp_registry.catalog_to_server_for_org(record.org_id);
        let dynamic = Arc::new(ScopedMcpSource::new(
            self.mcp_registry.clone(),
            &record.allowed_mcp_tools,
            &catalog_to_server,
        ));
        let toolbox = ToolBox::new(self.builtin_tools.clone(), dynamic);
        let model =
            self.model_resolver
                .resolve(record, self.providers.as_ref(), &self.provider_overlay);
        // Routed-provider attribution lands as a structured event so dashboards
        // can break down per-agent model selection over time. `patom.model.source`
        // is `"agent"` only when the resolved model matches the row's pin —
        // otherwise the resolver degraded a missing-provider pin back to the
        // default, and we want dashboards to see that as `"default"` so
        // degraded routing is visible, not hidden under the original pin
        // (CLAUDE.md §2; `patom.*` prefix for custom attributes).
        let source = if record.model == Some(model) {
            "agent"
        } else {
            "default"
        };
        // `patom.byo` records whether this org will route the resolved model's
        // provider through its own key — the same overlay read the Agent makes
        // live each turn (#141). Snapshot here for the resolution event; the
        // per-turn decision is re-read live by `Agent::is_byo`.
        let byo = self
            .provider_overlay
            .has_key(record.org_id, model.provider());
        tracing::info!(
            event = "agent.model.resolved",
            patom.agent.id = %record.id,
            patom.provider = model.provider().as_str(),
            patom.model = %model,
            patom.model.source = source,
            patom.byo = byo,
        );
        AgentBuilder::new(self.providers.clone(), self.memory.clone(), model)
            .expect("invariant: limits constants are static and parse")
            .with_org_routing(record.org_id, self.provider_overlay.clone())
            .with_tools(toolbox)
            .with_hooks(HookChain::new())
            .with_clock(self.clock.clone())
            .with_thread_store(self.threads.clone())
            .with_background_store(self.background.clone())
            .with_tool_call_store(self.tool_call_store.clone())
            .with_todos_store(self.todos_store.clone())
            .with_turn_metrics(
                self.turn_metrics_store.clone(),
                record.id,
                record.current_prompt_version_id,
            )
            .with_billing(self.billing.clone())
            .build()
    }
}

/// Aggregated handles passed to [`build_builtin_tools`]. Keeps the
/// `Collaborators::new` body under the §4 line cap by lifting tool
/// registration into its own function.
struct BuiltinToolDeps<'a> {
    http: Client,
    settings: &'a Settings,
    threads: crate::threads::SharedThreadStore,
    queue: SharedPromptQueue,
    dag: SharedDagBudget,
    agents: SharedAgentStore,
    colleagues: crate::colleagues::SharedColleagueStore,
    sink: SharedResponseSink,
    memory_tools: MemoryToolDeps,
    todo_tools: TodoToolDeps,
    embedding_provider: SharedEmbeddingProvider,
    scheduled_tasks: SharedScheduledTaskStore,
    default_tz: DefaultTimezone,
    clock: SharedClock,
    /// Pool handle threaded through to the scheduling tools so they can
    /// open `begin_as_user` tx for tenant-side visibility gating.
    pool: PgPool,
    /// Catalog store consumed by `search_tools` and `request_user_wire_mcp`.
    mcp_catalog: SharedMcpCatalogStore,
    /// MCP server store consumed by `search_tools` for the wired-vs-unwired
    /// view and by `request_user_wire_mcp` for the already-wired guard.
    mcp_store: SharedMcpServerStore,
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
            deps.threads.clone(),
            deps.queue.clone(),
            deps.dag.clone(),
            deps.agents.clone(),
            deps.colleagues.clone(),
            deps.sink.clone(),
        )))
        .with(Arc::new(MemoryWriteTool::new(deps.memory_tools.clone())))
        .with(Arc::new(MemoryUpdateTool::new(deps.memory_tools.clone())))
        .with(Arc::new(MemoryForgetTool::new(deps.memory_tools.clone())))
        .with(Arc::new(MemoryValidateTool::new(deps.memory_tools.clone())))
        .with(Arc::new(RecallTool::new(
            deps.memory_tools,
            deps.embedding_provider.clone(),
        )))
        .with(Arc::new(TodoWriteTool::new(deps.todo_tools)))
        .with(Arc::new(SearchAgentsTool::new(
            deps.agents.clone(),
            deps.embedding_provider,
        )))
        .with(Arc::new(CreateAgentTool::new(deps.agents.clone())))
        .with(Arc::new(SearchToolsTool::new(
            deps.mcp_catalog.clone(),
            deps.mcp_store.clone(),
        )))
        .with(Arc::new(RequestUserWireMcpTool::new(
            deps.mcp_catalog,
            deps.mcp_store,
            deps.sink.clone(),
        )))
        .with(Arc::new(ScheduleTaskTool::new(
            deps.scheduled_tasks.clone(),
            deps.threads.clone(),
            deps.default_tz,
            deps.clock,
        )))
        .with(Arc::new(ListScheduledTasksTool::new(
            deps.scheduled_tasks.clone(),
            deps.pool.clone(),
        )))
        .with(Arc::new(CancelScheduledTaskTool::new(
            deps.scheduled_tasks,
            deps.pool.clone(),
        )))
        .build())
}

/// Seed material for the per-org preset recruiter in `language`.
///
/// Exposed so the OAuth callback (`auth::callback`) can mint the recruiter
/// inside the just-created personal org. The role + description
/// bodies are pulled from the per-language [`Prompts`] registry — the
/// `recruiter` an `org_id` with `default_language='vi'` ends up with is
/// the Vietnamese-translated seed. Fails only if the registry bodies
/// suddenly violate a newtype invariant (a startup-time guarantee in
/// practice since `Prompts::load` panics on malformed input).
pub fn recruiter_seed(prompts: &Prompts, language: Language) -> Result<AgentSeed, AppError> {
    let set = prompts.set(language);
    Ok(AgentSeed {
        name: AgentName::try_from(RECRUITER_AGENT_NAME)?,
        system_prompt: AgentSystemPrompt::try_from(set.default_agent_role.as_ref())?,
        description: AgentDescription::try_from(set.default_agent_description.as_ref())?,
    })
}

/// Seed the preset recruiter for `org_id`.
///
/// Idempotent — a second call for the same org returns the existing
/// recruiter's id. Called from org creation so the fresh workspace has a
/// usable agent immediately.
pub async fn seed_recruiter_for_org(
    agents: &SharedAgentStore,
    org_id: OrgId,
    seed: AgentSeed,
) -> Result<crate::agents::AgentId, AgentStoreError> {
    agents.seed_preset(org_id, seed).await
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
        pieces.providers.clone(),
        pieces.memory.clone(),
        settings.model,
    )
    .expect("invariant: limits constants are static and parse")
    .with_tools(toolbox)
    .with_hooks(HookChain::new())
    .with_clock(pieces.clock.clone())
    .with_tool_call_store(tool_call_store)
    .with_todos_store(pieces.todos_store.clone())
    .build()
}

/// Inline `window.__PATOM_CONFIG__` into the SPA shell so the client reads
/// runtime analytics config synchronously, with no network roundtrip. The JSON
/// is built with `serde_json` (correct escaping of quotes/backslashes) and `</`
/// is escaped so an operator-supplied value can't close the `<script>` early.
/// Returns `raw_html` unchanged when it has no `</head>` (e.g. an empty shell).
fn inject_runtime_config(raw_html: &str, posthog_key: &str, posthog_host: &str) -> String {
    let config_json = serde_json::to_string(&serde_json::json!({
        "posthogKey": posthog_key,
        "posthogHost": posthog_host,
    }))
    .expect("invariant: static config object serializes");
    // Escape `</` so a value containing `</script>` can't close the tag early.
    let config_json = config_json.replace("</", "<\\/");
    let config_script = format!("<script>window.__PATOM_CONFIG__={config_json};</script>");
    raw_html.replace("</head>", &format!("{config_script}</head>"))
}

/// Build the full HTTP + worker pool composition. The returned [`Server`] is ready to
/// hand to `axum::serve` and a graceful-shutdown loop.
#[allow(clippy::too_many_lines)] // composition root: configuration + binding, not branching
pub async fn build_server(
    settings: Settings,
    entitlements: crate::entitlements::SharedEntitlements,
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
    let turn_metrics_store: crate::agent_core::turn_metrics::SharedTurnMetricsStore =
        Arc::new(crate::agent_core::turn_metrics::PgTurnMetricsStore::new(
            pieces.pool.clone(),
            pieces.clock.clone(),
        ));
    let model_resolver: crate::agents::SharedModelResolver =
        Arc::new(crate::agents::StaticAgentModelResolver::new(settings.model));
    // Per-org spend budget — a thin (pool, clock) wrapper, built here so every
    // agent the factory materialises shares one handle (CLAUDE.md §9).
    let billing: crate::billing::SharedBillingService =
        Arc::new(crate::billing::PgBillingService::with_entitlements(
            pieces.pool.clone(),
            pieces.clock.clone(),
            entitlements.clone(),
        ));
    let factory_pieces = AgentFactoryPieces {
        providers: pieces.providers.clone(),
        threads: pieces.threads.clone(),
        background: pieces.background.clone(),
        memory: pieces.memory.clone(),
        clock: pieces.clock.clone(),
        builtin_tools: pieces.builtin_tools.clone(),
        mcp_registry: pieces.mcp_registry.clone(),
        model_resolver,
        provider_overlay: pieces.provider_overlay.clone(),
        tool_call_store,
        todos_store: pieces.todos_store.clone(),
        turn_metrics_store,
        billing: billing.clone(),
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
    // BYO provider overlay (#141): same best-effort startup refresh so the
    // first turn already routes keyed providers.
    if let Err(e) = pieces.provider_overlay.refresh().await {
        warn!(error = %e, "provider.overlay.refresh.startup_failed");
    }

    let (mcp_refresher, mcp_refresh) = McpRefresher::spawn(pieces.mcp_registry.clone());
    let (provider_refresher, provider_refresh) =
        crate::provider::ProviderRefresher::spawn(pieces.provider_overlay.clone());

    let pool = WorkerPool::new(
        pieces.queue.clone(),
        pieces.sink.clone(),
        agents_registry,
        pieces.threads.clone(),
        pieces.dag.clone(),
        WorkerConfig::default(),
    );
    let workers = pool.spawn();

    // Background scheduler that periodically enqueues reflection turns
    // (doc/memory.md §1.6). The reflection itself runs through the worker
    // pool above.
    let reflection_scheduler = ReflectionScheduler::spawn(
        pieces.pool.clone(),
        pieces.queue.clone(),
        pieces.background.clone(),
        pieces.clock.clone(),
        cancel.clone(),
    );

    // Librarian — mechanical sweep per agent, plus resolution-turn
    // enqueue for unresolved contradictions (doc/memory.md §1.8).
    let librarian_scheduler = LibrarianScheduler::spawn(
        pieces.pool.clone(),
        pieces.memory_store.clone(),
        pieces.queue.clone(),
        pieces.background.clone(),
        pieces.clock.clone(),
        cancel.clone(),
    );

    // Scheduling — agent-driven scheduled tasks. Polls the
    // `scheduled_tasks` table on a fixed cadence and enqueues a
    // `prompt_requests` row for each due fire.
    let scheduling_scheduler = ScheduledTaskScheduler::spawn(
        pieces.scheduled_tasks.clone(),
        pieces.queue.clone(),
        pieces.threads.clone(),
        pieces.colleagues.clone(),
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
    // Discover the login IdP's endpoints + JWKS once at startup (§9,
    // static-at-startup; §5, timeout-bounded). Fail-closed: a provider we
    // can't discover aborts boot rather than degrading login. Google is
    // one issuer behind this seam (ADR-0011).
    let oauth: SharedOidcAuth = Arc::new(
        OidcProvider::discover(
            &settings.auth.oidc_issuer,
            &settings.auth.oidc_client_id,
            &settings.auth.oidc_client_secret,
            &settings.auth.oidc_redirect_url,
        )
        .await
        .map_err(AppError::Auth)?,
    );

    // Platform-supported MCP OAuth clients now resolve from env
    // (`PATOM_<X>_CLIENT_ID/_SECRET`) via `client_resolver::resolve` at
    // request time — no boot-time DB seeding. See
    // `src/mcp/oauth/client_resolver.rs`.

    let memberships = Arc::new(crate::http::MembershipCache::new(pieces.clock.clone()));
    let mcp_test_rate = crate::mcp::TestConnectRateLimiter::new(pieces.clock.clone());

    // Token refresh is now per-call inside `PatomMcpHttpClient` (the rmcp
    // transport adapter) — refresh-on-acquire + refresh-on-401. No
    // background task.

    // Slack adapter — built only when the operator has set the three
    // `PATOM_SLACK_*` env vars. We construct the stores, mint the
    // outbound poster, spawn the stream-pump supervisor and the
    // inbound bridge worker, then expose all of it via
    // `AppState::slack`. Without the env vars, every Slack route 404s
    // and no background tasks are spawned.
    let (slack_app_state, slack_bridge_handle) = match settings.slack.as_ref() {
        None => (None, None),
        Some(cfg) => {
            use crate::slack::bridge::BridgeDeps;
            use crate::slack::identity::PgSlackIdentityStore;
            use crate::slack::poster::{HttpSlackPoster, SharedSlackPoster};
            use crate::slack::state::SlackAppState;
            use crate::slack::stream_pump::PumpDeps;
            use crate::slack::thread_map::PgSlackThreadStore;
            use crate::slack::workspace::PgSlackWorkspaceStore;
            use crate::slack::{bridge, stream_pump};

            let workspaces = Arc::new(PgSlackWorkspaceStore::new(
                pieces.pool.clone(),
                pieces.clock.clone(),
                pieces.mcp_encryptor.clone(),
            ));
            let identities = Arc::new(PgSlackIdentityStore::new(
                pieces.pool.clone(),
                pieces.clock.clone(),
            ));
            let slack_channels = Arc::new(crate::slack::channel_map::PgSlackChannelStore::new(
                pieces.pool.clone(),
                pieces.clock.clone(),
            ));
            let threads_store = Arc::new(PgSlackThreadStore::new(
                pieces.pool.clone(),
                pieces.clock.clone(),
            ));
            let slack_http = build_http_client()?;
            let poster: SharedSlackPoster = Arc::new(HttpSlackPoster::new(slack_http.clone()));

            let pump_handle = stream_pump::spawn(
                PumpDeps {
                    thread_stream: thread_stream.clone(),
                    workspaces: workspaces.clone(),
                    agents: pieces.agents.clone(),
                    poster: poster.clone(),
                    signing_secret: cfg.signing_secret.clone(),
                    connect_url_base: Arc::from(settings.auth.oauth_redirect_base.as_str()),
                    clock: pieces.clock.clone(),
                },
                cancel.clone(),
            );

            let (bridge_handle, bridge_tx) = bridge::spawn(
                BridgeDeps {
                    queue: pieces.queue.clone(),
                    agents: pieces.agents.clone(),
                    thread_store: pieces.threads.clone(),
                    colleagues: pieces.colleagues.clone(),
                    workspaces: workspaces.clone(),
                    identities: identities.clone(),
                    channels_map: slack_channels.clone(),
                    threads: threads_store.clone(),
                    poster: poster.clone(),
                    stream_pump: pump_handle.clone(),
                    pool: pieces.pool.clone(),
                    http: slack_http.clone(),
                },
                cancel.clone(),
            );

            let state = SlackAppState {
                signing_secret: cfg.signing_secret.clone(),
                client_id: Arc::from(cfg.client_id.as_str()),
                client_secret: cfg.client_secret.clone(),
                redirect_url: Arc::from(cfg.redirect_url.as_str()),
                public_base_url: Arc::from(settings.auth.oauth_redirect_base.as_str()),
                workspaces,
                identities,
                threads: threads_store,
                poster,
                http: slack_http,
                bridge_tx,
                stream_pump: pump_handle,
                clock: pieces.clock.clone(),
            };
            (Some(state), Some(bridge_handle))
        }
    };

    // Object-storage seam — built once at startup from S3 settings when
    // present. Holding `None` is a first-class deployment shape; the
    // upload routes 503 cleanly instead of every other handler refusing
    // to start. CLAUDE.md §9: pool sized + endpoint resolved at boot.
    let assets: Option<SharedAssetStore> = settings.object_storage.as_ref().map(|cfg| {
        let store: SharedAssetStore = Arc::new(S3AssetStore::new(cfg));
        store
    });

    let orgs_store: crate::orgs::SharedOrgStore =
        Arc::new(crate::orgs::PgOrgStore::new(pieces.pool.clone()));
    // Mailer seam (issue #120). A configured SMTP relay delivers real invite
    // mail; with `PATOM_SMTP_*` unset we fall back to `LogMailer`, which keeps
    // the link recoverable from logs — a first-class shape for local dev and
    // relay-less deployments. A bad relay config fails fast here at startup.
    let mailer: crate::orgs::SharedMailer = match settings.smtp.as_ref() {
        Some(cfg) => Arc::new(crate::orgs::SmtpMailer::try_new(cfg)?),
        None => Arc::new(crate::orgs::LogMailer),
    };

    // Inject runtime config into index.html once at startup (GitLab pattern).
    // The SPA reads `window.__PATOM_CONFIG__` synchronously — no roundtrip.
    // A missing/unreadable index.html means the frontend wasn't built (an
    // API-only or `bun dev` backend, or a broken image); we warn and serve an
    // empty shell rather than fail boot, so the BE still runs in those cases.
    let index_path = settings.web_dist.join("index.html");
    let raw_html = match std::fs::read_to_string(&index_path) {
        Ok(html) => html,
        Err(error) => {
            warn!(
                ?error,
                path = %index_path.display(),
                "index.html unreadable; serving empty SPA shell (frontend not built?)"
            );
            String::new()
        }
    };
    let posthog_key = settings.posthog_key.as_deref().unwrap_or("");
    let posthog_host = settings
        .posthog_host
        .as_deref()
        .unwrap_or("https://eu.i.posthog.com");
    let index_html: Arc<str> =
        Arc::from(inject_runtime_config(&raw_html, posthog_key, posthog_host));

    let state = AppState {
        queue: pieces.queue,
        responses: pieces.responses,
        agents: pieces.agents,
        colleagues: pieces.colleagues,
        dag: pieces.dag,
        billing,
        memory_store: pieces.memory_store.clone(),
        mcp_store: pieces.mcp_store,
        mcp_catalog: pieces.mcp_catalog,
        mcp_credentials: pieces.mcp_credentials,
        mcp_refresh,
        provider_credentials: pieces.provider_credentials,
        provider_refresh,
        mcp_test_rate,
        platform_oauth_clients: pieces.platform_oauth_clients,
        mcp_oauth_pending: pieces.mcp_oauth_pending,
        oauth_redirect_base: Arc::from(settings.auth.oauth_redirect_base.as_str()),
        web_base_url: settings.auth.web_base_url.as_deref().map(Arc::from),
        thread_stream,
        pool: pieces.pool.clone(),
        jwt,
        oauth,
        bootstrap_admin: settings.auth.bootstrap_admin,
        // Build-mode cloud signal (issue: self-service workspaces). Set
        // from the `cloud` cargo feature at the composition root so
        // cloud-only behavior keys on the binary build, not an env flag.
        cloud: cfg!(feature = "cloud"),
        users: pieces.users,
        clock: pieces.clock.clone(),
        cookie_secure: settings.auth.cookie_secure,
        cookie_domain: settings.auth.cookie_domain.clone(),
        cors_allowed_origins: settings.auth.cors_allowed_origins.clone(),
        memberships,
        prompts: pieces.prompts,
        language_resolver: pieces.language_resolver,
        rule_resolver: pieces.rule_resolver,
        web_dist: settings.web_dist.clone(),
        index_html,
        slack: slack_app_state,
        assets,
        orgs: orgs_store,
        mailer,
        // Entitlement policy seam (#134/#154). Injected by the binary's
        // composition root: the OSS / self-host build passes the permissive
        // `UnlimitedEntitlements`; the cloud build (`--features cloud`) passes
        // `patom_cloud::CloudEntitlements`. Nothing below branches on the build.
        entitlements,
    };

    Ok(Server {
        state,
        workers,
        mcp_refresher,
        provider_refresher,
        reflection_scheduler,
        librarian_scheduler,
        scheduling_scheduler,
        http_addr: settings.http_addr,
        slack_bridge: slack_bridge_handle,
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
        provider_refresher,
        reflection_scheduler,
        librarian_scheduler,
        scheduling_scheduler,
        http_addr,
        slack_bridge,
    } = server;
    // The supervisor task that owns the stream-pump JoinSet is held
    // behind `state.slack`. Clone the handle out before the state
    // moves into the axum router so shutdown can still reach it.
    let slack_pump_handle = state.slack.as_ref().map(|s| s.stream_pump.clone());
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
    info!("mcp.refresher.shutdown.complete");
    provider_refresher.shutdown().await;
    info!("provider.overlay.refresher.shutdown.complete");
    if let Some(bridge) = slack_bridge {
        bridge.shutdown().await;
        info!("slack.bridge.shutdown.complete");
    }
    if let Some(pump) = slack_pump_handle {
        pump.shutdown().await;
        info!("slack.stream_pump.shutdown.complete");
    }
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

fn build_provider_registry(
    settings: &Settings,
) -> Result<crate::provider::SharedProviderRegistry, AppError> {
    use crate::provider::{ProviderId, ProviderRegistry};
    let mut builder = ProviderRegistry::builder();
    if let Some(c) = &settings.providers.anthropic {
        builder = builder.insert(
            ProviderId::Anthropic,
            Arc::new(AnthropicProvider::new(&c.api_key, c.base_url.clone())?),
        );
    }
    if let Some(c) = &settings.providers.openai {
        builder = builder.insert(
            ProviderId::Openai,
            Arc::new(OpenAiProvider::openai(&c.api_key, c.base_url.clone())),
        );
    }
    if let Some(c) = &settings.providers.deepseek {
        builder = builder.insert(
            ProviderId::Deepseek,
            Arc::new(OpenAiProvider::deepseek(&c.api_key, c.base_url.clone())),
        );
    }
    Ok(Arc::new(builder.build()))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inject_runtime_config_inserts_script_inside_head() {
        let out = inject_runtime_config(
            "<html><head><title>x</title></head><body></body></html>",
            "phc_abc",
            "https://eu.i.posthog.com",
        );
        assert!(out.contains(
            r#"window.__PATOM_CONFIG__={"posthogKey":"phc_abc","posthogHost":"https://eu.i.posthog.com"}"#
        ));
        // Injected before `</head>`, so the script lands inside `<head>`.
        let script_at = out.find("__PATOM_CONFIG__").expect("script present");
        let head_close = out.find("</head>").expect("head close present");
        assert!(script_at < head_close, "config script must precede </head>");
    }

    #[test]
    fn inject_runtime_config_escapes_script_breakout() {
        // A stray `</script>` in a value must not close the tag early.
        let out = inject_runtime_config("<head></head>", "</script><b>", "https://x.example");
        assert!(
            !out.contains(r#""</script><b>""#),
            "raw breakout must be escaped, got: {out}"
        );
        assert!(out.contains("<\\/script>"), "`</` should escape to `<\\/`");
    }

    #[test]
    fn inject_runtime_config_without_head_is_unchanged() {
        let raw = "<html><body>no head here</body></html>";
        assert_eq!(
            inject_runtime_config(raw, "phc_x", "https://x.example"),
            raw
        );
    }
}
