//! Worker-side write-RLS probe.
//!
//! Proves that the `_for_user` store paths the worker pool and the
//! tool layer now take are gated by Postgres RLS — a worker (or a
//! buggy tool) attempting to write into a foreign org's
//! `session_messages` row is rejected at the database boundary, not
//! merely by application logic.
//!
//! Both halves are exercised against real Postgres:
//!
//! 1. Negative: open `SessionStore::append_for_user(B_user, A_session, …)`.
//!    Org B's user is not a member of org A. The RLS WITH CHECK on
//!    `session_messages.org_id` filters the insert; the call surfaces
//!    a backend error (the trigger or the policy fires — either way
//!    the row never lands). We then read row-count = 1 (the seeded
//!    prompt's row, nothing else).
//! 2. Positive: same op with `A_user` succeeds and the row lands.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::sync::Arc;

use patom::agents::{
    AgentDescription, AgentName, AgentSystemPrompt, DefaultAgentSeed, PgAgentStore,
};
use patom::auth::{OrgId, UserId};
use patom::clock::SystemClock;
use patom::provider::ChatMessage;
use patom::runtime::PromptRequestId;
use patom::session::{PgSessionStore, SessionId, SessionStore};
use sqlx::PgPool;
use uuid::Uuid;

mod common;
use common::pg::{Seed, seed_tenant};

/// Seed a second org `(org_b, user_b)` into the test schema and a
/// default agent for it. Returns `(org_id, user_id, agent_id)`. The
/// schema is the same one `seed_tenant` minted; we just splice in
/// another tenant alongside the seeded one.
async fn seed_second_org(pool: &PgPool) -> (OrgId, UserId, patom::agents::AgentId) {
    let now = chrono::Utc::now();
    let org_id = OrgId::new();
    let user_id = UserId::new();
    let slug = format!("other-{}", &Uuid::new_v4().simple().to_string()[..8]);
    let email = format!(
        "other-{}@example.test",
        &Uuid::new_v4().simple().to_string()[..8]
    );
    sqlx::query("INSERT INTO users (id, email, display_name, created_at, updated_at) VALUES ($1, $2, $3, $4, $4)")
        .bind(user_id)
        .bind(&email)
        .bind("Other User")
        .bind(now)
        .execute(pool)
        .await
        .expect("seed other user");
    sqlx::query("INSERT INTO organizations (id, name, slug, default_language, created_at, updated_at) VALUES ($1, $2, $3, 'en', $4, $4)")
        .bind(org_id)
        .bind("Other Org")
        .bind(&slug)
        .bind(now)
        .execute(pool)
        .await
        .expect("seed other org");
    sqlx::query(
        "INSERT INTO org_members (org_id, user_id, role, created_at) VALUES ($1, $2, 'owner', $3)",
    )
    .bind(org_id)
    .bind(user_id)
    .bind(now)
    .execute(pool)
    .await
    .expect("seed other membership");

    // Mint a default agent in org B so the parity trigger has
    // something to bind against if the test ever reverses sides.
    let agents = Arc::new(PgAgentStore::new(
        pool.clone(),
        SystemClock::shared(),
        common::embedding::FakeEmbeddingProvider::shared(),
    ));
    let agent_id = agents
        .seed_default(
            org_id,
            DefaultAgentSeed {
                name: AgentName::try_from("other-default").expect("name"),
                system_prompt: AgentSystemPrompt::try_from("other org default").expect("prompt"),
                description: AgentDescription::try_from("Other org default agent.").expect("desc"),
            },
        )
        .await
        .expect("seed other default agent");
    (org_id, user_id, agent_id)
}

/// Mint a human↔agent session in org A and append the first message
/// through the privileged path so the session has a known row count
/// baseline of 1. Returns the session id.
async fn seed_session_in_org_a(
    sessions: &dyn SessionStore,
    pool: &PgPool,
    seed: &Seed,
) -> (SessionId, PromptRequestId) {
    let session = common::pg::human_to_agent_session(
        pool,
        sessions,
        seed.agent_id,
        seed.org_id,
        seed.user_id,
    )
    .await;
    let request_id =
        common::pg::seed_prompt_request(pool, session, seed.agent_id, seed.org_id).await;
    sessions
        .append_for_user(
            seed.user_id,
            session,
            common::pg::human_sender(pool, seed.org_id, seed.user_id).await,
            common::pg::agent_participant(pool, seed.org_id, seed.agent_id).await,
            ChatMessage::User(vec![patom::provider::UserContent::Text("seed".to_string())]),
            request_id,
        )
        .await
        .expect("seed first message under A's user");
    (session, request_id)
}

async fn message_count(pool: &PgPool, session: SessionId) -> i64 {
    let (count,): (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM session_messages WHERE session_id = $1")
            .bind(session)
            .fetch_one(pool)
            .await
            .expect("count rows");
    count
}

#[sqlx::test]
async fn worker_write_to_foreign_org_session_fails_under_rls(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let sessions: Arc<dyn SessionStore> =
        Arc::new(PgSessionStore::new(pool.clone(), SystemClock::shared()));
    let (_, user_b, _) = seed_second_org(&pool).await;
    let (session, request_id) = seed_session_in_org_a(sessions.as_ref(), &pool, &seed).await;
    assert_eq!(message_count(&pool, session).await, 1);

    // Cross-tenant write: user B claims to act on a session owned by
    // org A. Under RLS the WITH CHECK on `session_messages.org_id`
    // rejects the insert — the call must surface an error and the
    // row count must remain unchanged.
    let result = sessions
        .append_for_user(
            user_b,
            session,
            common::pg::human_sender(&pool, seed.org_id, seed.user_id).await,
            common::pg::agent_participant(&pool, seed.org_id, seed.agent_id).await,
            ChatMessage::User(vec![patom::provider::UserContent::Text(
                "foreign".to_string(),
            )]),
            request_id,
        )
        .await;
    assert!(
        result.is_err(),
        "cross-org append must be rejected by RLS, got {result:?}",
    );
    assert_eq!(
        message_count(&pool, session).await,
        1,
        "row count must not have changed",
    );
}

#[sqlx::test]
async fn worker_write_under_correct_user_succeeds(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let sessions: Arc<dyn SessionStore> =
        Arc::new(PgSessionStore::new(pool.clone(), SystemClock::shared()));
    let (session, request_id) = seed_session_in_org_a(sessions.as_ref(), &pool, &seed).await;
    assert_eq!(message_count(&pool, session).await, 1);

    // Same operation with the legitimate principal succeeds and the
    // row count advances.
    sessions
        .append_for_user(
            seed.user_id,
            session,
            common::pg::human_sender(&pool, seed.org_id, seed.user_id).await,
            common::pg::agent_participant(&pool, seed.org_id, seed.agent_id).await,
            ChatMessage::User(vec![patom::provider::UserContent::Text(
                "legit".to_string(),
            )]),
            request_id,
        )
        .await
        .expect("legitimate append succeeds");
    assert_eq!(message_count(&pool, session).await, 2);
}
