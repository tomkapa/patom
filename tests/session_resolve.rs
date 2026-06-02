//! Trait-contract tests for [`SessionStore::resolve_or_create_for_pair`].
//!
//! Covers the spec's "session id stability" guarantee: two callers naming
//! the same `(root_request_id, canonical(a, b))` always converge on the
//! same session row, and different DAGs (different `root_request_id`s)
//! get distinct rows even when the participant pair matches.

#![allow(clippy::expect_used)]

use std::sync::Arc;

use patom::agents::{AgentName, AgentSystemPrompt, NewAgent, SharedAgentStore};
use patom::clock::SystemClock;
use patom::runtime::PromptRequestId;
use patom::session::{PgSessionStore, SessionStore};
use patom::types::Participant;
use sqlx::PgPool;

mod common;
use common::pg::seed_tenant;

fn store(pool: &PgPool) -> Arc<PgSessionStore> {
    Arc::new(PgSessionStore::new(pool.clone(), SystemClock::shared()))
}

/// Create a real agent row so FK-bearing inserts (sessions / session_messages)
/// can reference it. Returns the [`Participant::Agent`] wrapping its id.
async fn fresh_agent(pool: &PgPool, seed: &common::pg::Seed, name: &str) -> Participant {
    let store: SharedAgentStore =
        common::pg::shared_agent_store(pool.clone(), SystemClock::shared());
    let record = store
        .create(NewAgent {
            org_id: seed.org_id,
            name: AgentName::try_from(name).expect("name"),
            system_prompt: AgentSystemPrompt::try_from("test prompt").expect("prompt"),
            description: patom::agents::AgentDescription::try_from("test desc").expect("desc"),
            is_default: false,
            allowed_mcp_tools: patom::agents::AllowedMcpTools::empty(),
            model: None,
            avatar_url: None,
            edited_by: None,
        })
        .await
        .expect("create agent");
    Participant::agent(record.id)
}

#[sqlx::test]
async fn same_pair_same_dag_returns_same_session(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let store = store(&pool);
    let root = PromptRequestId::new();
    let a = Participant::Human;
    let b = Participant::agent(seed.agent_id);

    let first = store
        .resolve_or_create_for_pair(root, a, b, None, seed.org_id, seed.user_id)
        .await
        .expect("first");
    let second = store
        .resolve_or_create_for_pair(root, a, b, None, seed.org_id, seed.user_id)
        .await
        .expect("second");
    assert_eq!(first, second, "upsert is idempotent on same key");
}

#[sqlx::test]
async fn reversed_pair_canonicalises_to_same_session(pool: PgPool) {
    // Caller may pass `(a, b)` either way round; the store canonicalises so
    // both orderings hit the same `sessions_dag_pair_unique` index entry.
    let seed = seed_tenant(&pool).await;
    let store = store(&pool);
    let root = PromptRequestId::new();
    let h = Participant::Human;
    let a = Participant::agent(seed.agent_id);

    let forward = store
        .resolve_or_create_for_pair(root, h, a, None, seed.org_id, seed.user_id)
        .await
        .expect("forward");
    let reversed = store
        .resolve_or_create_for_pair(root, a, h, None, seed.org_id, seed.user_id)
        .await
        .expect("reversed");
    assert_eq!(forward, reversed);
}

#[sqlx::test]
async fn different_dags_get_distinct_sessions(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let store = store(&pool);
    let pair = (Participant::Human, Participant::agent(seed.agent_id));

    let dag_a = PromptRequestId::new();
    let dag_b = PromptRequestId::new();
    let s_a = store
        .resolve_or_create_for_pair(dag_a, pair.0, pair.1, None, seed.org_id, seed.user_id)
        .await
        .expect("dag_a");
    let s_b = store
        .resolve_or_create_for_pair(dag_b, pair.0, pair.1, None, seed.org_id, seed.user_id)
        .await
        .expect("dag_b");
    assert_ne!(s_a, s_b, "DAG isolation: same pair, different roots");
}

#[sqlx::test]
async fn parent_session_is_recorded(pool: PgPool) {
    // Forked sessions (e.g. agent A spawns conversation with agent B) carry
    // their parent so the agent loop can auto-load it on the receiver's
    // first turn.
    let seed = seed_tenant(&pool).await;
    let store = store(&pool);
    let root = PromptRequestId::new();
    let agent_a = Participant::agent(seed.agent_id);
    let agent_b = fresh_agent(&pool, &seed, "second").await;

    let parent_id = store
        .resolve_or_create_for_pair(
            root,
            Participant::Human,
            agent_a,
            None,
            seed.org_id,
            seed.user_id,
        )
        .await
        .expect("parent");
    let child_id = store
        .resolve_or_create_for_pair(
            root,
            agent_a,
            agent_b,
            Some(parent_id),
            seed.org_id,
            seed.user_id,
        )
        .await
        .expect("child");

    let recovered = store.parent(child_id).await.expect("parent lookup");
    assert_eq!(recovered, Some(parent_id));
    let root_parent = store.parent(parent_id).await.expect("root lookup");
    assert_eq!(root_parent, None, "root session has no parent");
}

#[sqlx::test]
async fn participants_are_returned_in_canonical_order(pool: PgPool) {
    // Agent < Human by canonical_cmp (matches SQL string-compare on the
    // *_kind columns), so participants() returns (Agent(_), Human)
    // regardless of which side the caller passed first.
    let seed = seed_tenant(&pool).await;
    let store = store(&pool);
    let root = PromptRequestId::new();
    let agent_p = Participant::agent(seed.agent_id);

    let id = store
        .resolve_or_create_for_pair(
            root,
            Participant::Human,
            agent_p,
            None,
            seed.org_id,
            seed.user_id,
        )
        .await
        .expect("session");
    let (a, b) = store.participants(id).await.expect("participants");
    assert_eq!(a, agent_p);
    assert_eq!(b, Participant::Human);
}

#[sqlx::test]
async fn root_request_id_round_trips(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let store = store(&pool);
    let root = PromptRequestId::new();
    let id = store
        .resolve_or_create_for_pair(
            root,
            Participant::Human,
            Participant::agent(seed.agent_id),
            None,
            seed.org_id,
            seed.user_id,
        )
        .await
        .expect("session");
    let resolved = store.root_request_id(id).await.expect("root");
    assert_eq!(resolved, root);
}

#[sqlx::test]
async fn self_session_is_rejected(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let store = store(&pool);
    let root = PromptRequestId::new();
    let err = store
        .resolve_or_create_for_pair(
            root,
            Participant::agent(seed.agent_id),
            Participant::agent(seed.agent_id),
            None,
            seed.org_id,
            seed.user_id,
        )
        .await
        .expect_err("self-session forbidden");
    assert!(matches!(err, patom::session::SessionError::SelfSession));
}
