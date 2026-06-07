//! Per-test Postgres seeding for the `#[sqlx::test]` harness.
//!
//! Isolation, migration, and teardown are owned by sqlx: `#[sqlx::test]` mints a
//! fresh database per test, runs `migrations/` into it (the fresh database's
//! default `search_path` is `public`, so the `CREATE EXTENSION` migrations land
//! in `public` and resolve for every query — no schema-pinning trap), injects a
//! [`PgPool`], and drops the database afterward, reaping stragglers on the next
//! run. This module only provides the *seeding* every test needs on top of an
//! empty-but-migrated database.
//!
//! - Env: `#[sqlx::test]` reads `DATABASE_URL` (and `.env`) to locate the server
//!   it creates per-test databases under.
//! - The `patom` role must be able to `CREATE DATABASE` (it is the
//!   `POSTGRES_USER`, so it can).

use std::sync::Arc;

use patom::agents::{
    AgentDescription, AgentId, AgentName, AgentSystemPrompt, DefaultAgentSeed, PgAgentStore,
    SharedAgentStore,
};
use patom::auth::{OrgId, UserId};
use patom::clock::{SharedClock, SystemClock};
use patom::runtime::PromptRequestId;
use patom::session::{SessionId, SessionStore};
use patom::types::Participant;
use sqlx::PgPool;
use uuid::Uuid;

/// The default tenant seeded into a freshly-migrated test database by
/// [`seed_tenant`]: one user, one org, an Owner membership, and one default
/// agent scoped to the org.
///
/// `agent_id` exists so `sessions.agent_id NOT NULL REFERENCES agents(id)` can
/// be satisfied without ceremony; `org_id` gives RLS-bound table tests (like
/// `mcp_servers`) a valid tenant to insert against; `user_id` owns the org and
/// is the actor for HTTP-layer probes (see `tests/common/auth.rs`).
// The `_id` suffix on every field is the codebase-wide convention for typed
// ids (§1); a non-id-suffixed name would be the surprise here.
#[allow(clippy::struct_field_names)]
#[derive(Debug, Clone, Copy)]
pub struct Seed {
    pub agent_id: AgentId,
    pub org_id: OrgId,
    pub user_id: UserId,
}

/// Seed the default tenant into a freshly-migrated database. Replaces the
/// old `TestDb::fresh` seeding; the database itself is provided by
/// `#[sqlx::test]`. Panics on any failure — a test cannot proceed without its
/// tenant.
pub async fn seed_tenant(pool: &PgPool) -> Seed {
    // Seed an org + user up front so RLS-bound table tests have a valid
    // `org_id` to insert against without minting their own.
    let user_id = UserId::new();
    let org_id = OrgId::new();
    let now = chrono::Utc::now();
    let user_email = format!("seed-{}@example.test", Uuid::new_v4().simple());
    let org_slug = format!("seed-{}", &Uuid::new_v4().simple().to_string()[..8]);
    sqlx::query("INSERT INTO users (id, email, display_name, created_at, updated_at) VALUES ($1, $2, $3, $4, $4)")
        .bind(user_id)
        .bind(&user_email)
        .bind("Seeded Test User")
        .bind(now)
        .execute(pool)
        .await
        .expect("seed user");
    sqlx::query("INSERT INTO organizations (id, name, slug, default_language, created_at, updated_at) VALUES ($1, $2, $3, 'en', $4, $4)")
        .bind(org_id)
        .bind("Seeded Test Org")
        .bind(&org_slug)
        .bind(now)
        .execute(pool)
        .await
        .expect("seed org");
    sqlx::query(
        "INSERT INTO org_members (org_id, user_id, role, created_at) VALUES ($1, $2, 'owner', $3)",
    )
    .bind(org_id)
    .bind(user_id)
    .bind(now)
    .execute(pool)
    .await
    .expect("seed membership");

    // Seed a default agent so `sessions.agent_id` (NOT NULL REFERENCES
    // agents) can be satisfied by tests calling `sessions.create(id)`.
    // The seed is scoped to `org_id` so `agents.default_id_for` resolves for
    // tests that go through the HTTP path with a principal pinned to the same
    // org.
    let agents = agent_store(pool.clone(), SystemClock::shared());
    let agent_id = agents
        .seed_default(
            org_id,
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

    Seed {
        agent_id,
        org_id,
        user_id,
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
/// caller passes [`Seed::org_id`] / [`Seed::user_id`] because the seeded
/// [`Seed::agent_id`] lives in that org and the trigger on `sessions` would
/// reject a cross-org `(agent, org)` pair.
///
/// The synthetic `root_request_id` is generated locally; nothing dereferences
/// it (no FK from `sessions.root_request_id` to `prompt_requests.id`), but
/// integration tests that also exercise `prompt_requests` should mint a real
/// request id and pass it in instead.
pub async fn human_to_agent_session(
    pool: &PgPool,
    sessions: &dyn SessionStore,
    agent_id: AgentId,
    org_id: OrgId,
    user_id: UserId,
) -> SessionId {
    let root = PromptRequestId::new();
    let (human, agent) = human_to_agent_pair(pool, org_id, user_id, agent_id).await;
    sessions
        .resolve_or_create_for_pair(root, human, agent, None, org_id, user_id)
        .await
        .expect("create human-to-agent session")
}

/// Resolve a colleague-backed `(Human, Agent)` participant pair from a seeded
/// tenant. Mirrors the boundary callers (HTTP / Slack / scheduler) — every
/// participant a session stores must reference a colleague row.
pub async fn human_to_agent_pair(
    pool: &PgPool,
    org_id: OrgId,
    user_id: UserId,
    agent_id: AgentId,
) -> (Participant, Participant) {
    let human_colleague = patom::colleagues::resolve_user_colleague(pool, org_id, user_id)
        .await
        .expect("seed mints human colleague");
    let agent_colleague = patom::colleagues::resolve_agent_colleague(pool, org_id, agent_id)
        .await
        .expect("seed mints agent colleague");
    (
        Participant::human(human_colleague, user_id),
        Participant::agent(agent_colleague, agent_id),
    )
}

/// Convenience: build just the human-side colleague-backed `Participant`.
pub async fn human_participant(pool: &PgPool, org_id: OrgId, user_id: UserId) -> Participant {
    let cid = patom::colleagues::resolve_user_colleague(pool, org_id, user_id)
        .await
        .expect("seed mints human colleague");
    Participant::human(cid, user_id)
}

/// Convenience: build just the agent-side colleague-backed `Participant`.
pub async fn agent_participant(pool: &PgPool, org_id: OrgId, agent_id: AgentId) -> Participant {
    let cid = patom::colleagues::resolve_agent_colleague(pool, org_id, agent_id)
        .await
        .expect("seed mints agent colleague");
    Participant::agent(cid, agent_id)
}

/// Convenience: build the human-side colleague-backed `MessageSender`.
pub async fn human_sender(
    pool: &PgPool,
    org_id: OrgId,
    user_id: UserId,
) -> patom::types::MessageSender {
    let cid = patom::colleagues::resolve_user_colleague(pool, org_id, user_id)
        .await
        .expect("seed mints human colleague");
    patom::types::MessageSender::Human {
        colleague_id: cid,
        user_id,
    }
}

/// Configure (or update) an org's spend budget. `cap` of `None` is the
/// unlimited case. Runs as the table owner (RLS-bypassing in the dev/test
/// image), mirroring [`seed_tenant`].
pub async fn set_budget(pool: &PgPool, org: OrgId, cap: Option<i64>, bps: i32) {
    sqlx::query(
        "INSERT INTO org_budgets (org_id, monthly_cap_micro_usd, warn_threshold_bps, created_at, updated_at)
         VALUES ($1, $2, $3, now(), now())
         ON CONFLICT (org_id) DO UPDATE
             SET monthly_cap_micro_usd = EXCLUDED.monthly_cap_micro_usd,
                 warn_threshold_bps    = EXCLUDED.warn_threshold_bps,
                 updated_at            = EXCLUDED.updated_at",
    )
    .bind(org)
    .bind(cap)
    .bind(bps)
    .execute(pool)
    .await
    .expect("set budget");
}

/// Seed `used` micro-USD of spend for `org` in the current UTC month — used to
/// drive an org over its cap before exercising a gate.
pub async fn seed_period_usage(pool: &PgPool, org: OrgId, used: i64) {
    sqlx::query(
        "INSERT INTO org_budget_usage (org_id, period_start, used_micro_usd, created_at, updated_at)
         VALUES ($1, date_trunc('month', now())::date, $2, now(), now())",
    )
    .bind(org)
    .bind(used)
    .execute(pool)
    .await
    .expect("seed period usage");
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
    // After migration 60 the participant columns are colleague-backed. The
    // shared seed uses the freshly-seeded human's colleague as sender and
    // the agent's colleague as receiver (both resolved by mint trigger from
    // migration 58).
    let result = sqlx::query(
        "INSERT INTO prompt_requests
             (id, session_id, org_id, content, idempotency_key, status,
              sender_colleague_id, receiver_colleague_id, root_request_id,
              created_at, updated_at)
         SELECT $1, $2, $3, 'test', $4, 'pending',
                hc.id, ac.id, $1,
                $6, $6
           FROM colleagues hc, colleagues ac
          WHERE hc.org_id = $3 AND hc.user_id IS NOT NULL
            AND ac.org_id = $3 AND ac.agent_id = $5
          LIMIT 1",
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
    // The `INSERT ... SELECT ... LIMIT 1` inserts zero rows if the seed
    // colleagues are missing, yet would still return `id` — a silent
    // dangling reference. Assert the row actually landed.
    assert_eq!(
        result.rows_affected(),
        1,
        "seed_prompt_request inserted no row — seed colleagues (human + agent) must exist for org"
    );
    id
}
