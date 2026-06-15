//! `PgLarkDirectory` directory-resolution tests.
//!
//! Focused on `agent_name_for`: the outbound render uses it to fall back to a
//! plain `@Name` text marker when a `send_message` addresses a peer *agent*
//! (which cannot be `<at>`-pinged across BYO apps). It must resolve an agent
//! colleague to its `agents.name` and return `None` for a human colleague.

#![allow(clippy::expect_used)]

use sqlx::PgPool;

use patom::clock::{SharedClock, SystemClock};
use patom::colleagues::{resolve_agent_colleague, resolve_user_colleague};
use patom::lark::directory::{LarkDirectory, PgLarkDirectory};

mod common;
use common::pg::seed_tenant;

#[sqlx::test]
async fn agent_name_for_resolves_agent_colleague_name(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let clock: SharedClock = SystemClock::shared();
    let directory = PgLarkDirectory::new(pool.clone(), clock);

    let agent_colleague = resolve_agent_colleague(&pool, seed.org_id, seed.agent_id)
        .await
        .expect("seeded agent has a colleague");

    let name = directory
        .agent_name_for(seed.org_id, agent_colleague)
        .await
        .expect("agent_name_for query");

    assert_eq!(
        name.as_deref(),
        Some("test-default"),
        "an agent colleague resolves to its agents.name"
    );
}

#[sqlx::test]
async fn agent_name_for_returns_none_for_human(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let clock: SharedClock = SystemClock::shared();
    let directory = PgLarkDirectory::new(pool.clone(), clock);

    let human_colleague = resolve_user_colleague(&pool, seed.org_id, seed.user_id)
        .await
        .expect("seeded member has a colleague");

    let name = directory
        .agent_name_for(seed.org_id, human_colleague)
        .await
        .expect("agent_name_for query");

    assert_eq!(name, None, "a human colleague is not an agent → None");
}

#[sqlx::test]
async fn agent_name_for_is_org_scoped(pool: PgPool) {
    // The agent colleague exists, but a query under a *different* org must not
    // see it — the lookup is org-scoped (RLS-bypassing but org-bound by $1).
    let seed = seed_tenant(&pool).await;
    let other_org = patom::auth::OrgId::new();
    let clock: SharedClock = SystemClock::shared();
    let directory = PgLarkDirectory::new(pool.clone(), clock);

    let agent_colleague = resolve_agent_colleague(&pool, seed.org_id, seed.agent_id)
        .await
        .expect("seeded agent has a colleague");

    let name = directory
        .agent_name_for(other_org, agent_colleague)
        .await
        .expect("agent_name_for query");

    assert_eq!(name, None, "an agent colleague is invisible to another org");
}
