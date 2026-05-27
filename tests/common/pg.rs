//! Schema-per-test Postgres harness.
//!
//! Each [`TestDb::fresh`] call mints a unique schema name, runs `migrations/` into it
//! by way of a pool whose `search_path` is pinned to that schema, and drops the
//! schema on teardown. Suitable for parallel `cargo test` runs against a single
//! long-running container brought up by the project's `docker-compose.yml`.
//!
//! - Env: set `DATABASE_URL` to override the default
//!   `postgres://patom:patom@localhost:5432/patom`.
//! - Cleanup is RAII: schemas drop synchronously when [`TestDb`] goes out of scope.
//!   The [`Drop`] impl uses `tokio::task::block_in_place` + `Handle::block_on`, so
//!   tests **must** run on the multi-threaded runtime
//!   (`#[tokio::test(flavor = "multi_thread")]`); a current-thread runtime panics on
//!   `block_in_place`. Stray schemas can be reaped manually with `psql`:
//!   `SELECT 'DROP SCHEMA "' || nspname || '" CASCADE;' FROM pg_namespace WHERE nspname LIKE 'patom_test_%';`

use std::time::Duration;

use std::sync::Arc;

use patom_rs::agents::{
    AgentDescription, AgentId, AgentName, AgentSystemPrompt, DefaultAgentSeed, PgAgentStore,
    SharedAgentStore,
};
use patom_rs::auth::{OrgId, UserId};
use patom_rs::clock::{SharedClock, SystemClock};
use patom_rs::runtime::PromptRequestId;
use patom_rs::session::{SessionId, SessionStore};
use patom_rs::types::Participant;
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;
use uuid::Uuid;

const DEFAULT_DATABASE_URL: &str = "postgres://patom:patom@localhost:5432/patom";
const TEARDOWN_TIMEOUT: Duration = Duration::from_secs(5);

/// Owns a freshly-migrated, schema-isolated Postgres pool. Use `pool` to construct
/// the unit under test (`PgSessionStore`, `PgPromptQueue`, `PgResponseHub`).
///
/// `default_agent_id` is the row seeded by [`TestDb::fresh`] — every fresh db
/// has exactly one default agent so `sessions.agent_id NOT NULL REFERENCES
/// agents(id)` can be satisfied by tests without ceremony.
pub struct TestDb {
    /// Pool whose connections are pinned to this test's schema via
    /// `SET search_path TO "<schema>", public`.
    pub pool: PgPool,
    pub default_agent_id: AgentId,
    /// Seeded org id — present on every freshly-created test schema so
    /// store-level tests that insert into RLS-bound tables (like
    /// `mcp_servers`) have a valid `org_id` to use without minting
    /// their own.
    pub default_org_id: OrgId,
    /// Owner of `default_org_id`. Tests that need a `Principal` for the
    /// HTTP-layer probes use this id; see `tests/common/auth.rs` for
    /// the cookie helper.
    pub default_user_id: UserId,
    schema: String,
    admin: PgPool,
}

impl TestDb {
    /// Mint a unique schema, build a pool pinned to it, and run the project's
    /// migrations into it. Panics on any setup failure — tests cannot proceed
    /// without a working database.
    pub async fn fresh() -> Self {
        let url = std::env::var("DATABASE_URL").unwrap_or_else(|_| DEFAULT_DATABASE_URL.into());

        // Admin pool keeps a connection open for the lifetime of the test so we can
        // CREATE / DROP SCHEMA without interfering with the test pool.
        let admin: PgPool = PgPoolOptions::new()
            .max_connections(2)
            .acquire_timeout(Duration::from_secs(5))
            .connect(&url)
            .await
            .expect("admin pool connect");

        let schema = format!("patom_test_{}", Uuid::new_v4().simple());
        sqlx::query(&format!("CREATE SCHEMA \"{schema}\""))
            .execute(&admin)
            .await
            .expect("create schema");

        let pinned_schema = schema.clone();
        let pool: PgPool = PgPoolOptions::new()
            .max_connections(8)
            .acquire_timeout(Duration::from_secs(5))
            .after_connect(move |conn, _meta| {
                let stmt = format!("SET search_path TO \"{pinned_schema}\", public");
                Box::pin(async move {
                    use sqlx::Executor as _;
                    conn.execute(stmt.as_str()).await?;
                    Ok(())
                })
            })
            .connect(&url)
            .await
            .expect("test pool connect");

        sqlx::migrate!("./migrations")
            .run(&pool)
            .await
            .expect("run migrations");

        // Seed an org + user up front so RLS-bound table tests have a
        // valid `org_id` to insert against without minting their own.
        let default_user_id = UserId::new();
        let default_org_id = OrgId::new();
        let now = chrono::Utc::now();
        let user_email = format!("seed-{}@example.test", Uuid::new_v4().simple());
        let org_slug = format!("seed-{}", &Uuid::new_v4().simple().to_string()[..8]);
        sqlx::query("INSERT INTO users (id, email, display_name, created_at, updated_at) VALUES ($1, $2, $3, $4, $4)")
            .bind(default_user_id)
            .bind(&user_email)
            .bind("Seeded Test User")
            .bind(now)
            .execute(&pool)
            .await
            .expect("seed user");
        sqlx::query("INSERT INTO organizations (id, name, slug, default_language, created_at, updated_at) VALUES ($1, $2, $3, 'en', $4, $4)")
            .bind(default_org_id)
            .bind("Seeded Test Org")
            .bind(&org_slug)
            .bind(now)
            .execute(&pool)
            .await
            .expect("seed org");
        sqlx::query("INSERT INTO org_members (org_id, user_id, role, created_at) VALUES ($1, $2, 'owner', $3)")
            .bind(default_org_id)
            .bind(default_user_id)
            .bind(now)
            .execute(&pool)
            .await
            .expect("seed membership");

        // Seed a default agent so `sessions.agent_id` (NOT NULL REFERENCES
        // agents) can be satisfied by tests calling `sessions.create(id)`.
        // The seed is scoped to `default_org_id` so `agents.default_id_for`
        // resolves for tests that go through the HTTP path with a principal
        // pinned to the same org.
        let agents = agent_store(pool.clone(), SystemClock::shared());
        let default_agent_id = agents
            .seed_default(
                default_org_id,
                DefaultAgentSeed {
                    name: AgentName::try_from("test-default").expect("valid name"),
                    system_prompt: AgentSystemPrompt::try_from("test default prompt")
                        .expect("valid prompt"),
                    description: AgentDescription::try_from("Default test agent.")
                        .expect("valid description"),
                },
            )
            .await
            .expect("seed default agent");

        Self {
            pool,
            default_agent_id,
            default_org_id,
            default_user_id,
            schema,
            admin,
        }
    }
}

/// Construct a `PgAgentStore` wired with the fake embedding provider used
/// by every test path. Returns the concrete `Arc<PgAgentStore>`; callers
/// that need the trait object can coerce with `as SharedAgentStore`.
pub fn agent_store(pool: PgPool, clock: SharedClock) -> Arc<PgAgentStore> {
    Arc::new(PgAgentStore::new(
        pool,
        clock,
        super::embedding::FakeEmbeddingProvider::shared(),
    ))
}

/// `SharedAgentStore`-typed handle for callers that want the trait object
/// directly (most route / harness setups).
pub fn shared_agent_store(pool: PgPool, clock: SharedClock) -> SharedAgentStore {
    agent_store(pool, clock)
}

/// Mint a fresh human-to-`agent_id` session via the new
/// [`SessionStore::resolve_or_create_for_pair`] API. Tests use this to obtain
/// a session without going through the queue.
///
/// `org_id` / `user_id` pin the row to the seeded test tenant — every
/// caller passes [`TestDb::default_org_id`] / [`TestDb::default_user_id`]
/// because the seeded `default_agent_id` lives in that org and the
/// trigger on `sessions` would reject a cross-org `(agent, org)` pair.
///
/// The synthetic `root_request_id` is generated locally; nothing dereferences
/// it (no FK from `sessions.root_request_id` to `prompt_requests.id`), but
/// integration tests that also exercise `prompt_requests` should mint a real
/// request id and pass it in instead.
pub async fn human_to_agent_session(
    sessions: &dyn SessionStore,
    agent_id: AgentId,
    org_id: OrgId,
    user_id: UserId,
) -> SessionId {
    let root = PromptRequestId::new();
    sessions
        .resolve_or_create_for_pair(
            root,
            Participant::Human,
            Participant::agent(agent_id),
            None,
            org_id,
            user_id,
        )
        .await
        .expect("create human-to-agent session")
}

/// Insert a stub `prompt_requests` row for a freshly minted session and
/// return its id. `session_messages.request_id` is `NOT NULL REFERENCES
/// prompt_requests(id)` — store-level tests that exercise `append` need a
/// real request id to bind, even though they don't go through the queue.
///
/// All optional columns are filled with placeholders; the helper is for
/// store contract tests, not queue tests.
pub async fn seed_prompt_request(
    pool: &PgPool,
    session: SessionId,
    agent_id: AgentId,
    org_id: OrgId,
) -> PromptRequestId {
    let id = PromptRequestId::new();
    let now = chrono::Utc::now();
    sqlx::query(
        "INSERT INTO prompt_requests
             (id, session_id, org_id, content, idempotency_key, status,
              sender_kind, receiver_kind, receiver_agent_id, root_request_id,
              created_at, updated_at)
         VALUES ($1, $2, $3, 'test', $4, 'pending',
                 'human', 'agent', $5, $1,
                 $6, $6)",
    )
    .bind(id)
    .bind(session)
    .bind(org_id)
    .bind(format!("k-{id}"))
    .bind(agent_id)
    .bind(now)
    .execute(pool)
    .await
    .expect("seed prompt_request");
    id
}

impl Drop for TestDb {
    fn drop(&mut self) {
        let schema = std::mem::take(&mut self.schema);
        if schema.is_empty() {
            return;
        }
        // Drop runs while the test future is still being torn down on a runtime worker;
        // `Handle::try_current()` therefore succeeds on every supported test path.
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let admin = self.admin.clone();
        let pool = self.pool.clone();
        // `block_in_place` requires the multi-threaded runtime; tests pin
        // `flavor = "multi_thread"` so block_on the cleanup synchronously here.
        tokio::task::block_in_place(move || {
            handle.block_on(async move {
                pool.close().await;
                let _ = tokio::time::timeout(
                    TEARDOWN_TIMEOUT,
                    sqlx::query(&format!("DROP SCHEMA \"{schema}\" CASCADE")).execute(&admin),
                )
                .await;
                admin.close().await;
            });
        });
    }
}
