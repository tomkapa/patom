//! Trait-contract tests for [`SessionStore::resolve_or_create_for_pair`].
//!
//! Covers the spec's "session id stability" guarantee: two callers naming
//! the same `(root_request_id, canonical(a, b))` always converge on the
//! same session row, and different DAGs (different `root_request_id`s)
//! get distinct rows even when the participant pair matches.

#![allow(clippy::expect_used)]

use std::sync::Arc;

use patom_rs::agents::{AgentName, AgentSystemPrompt, NewAgent, SharedAgentStore};
use patom_rs::clock::SystemClock;
use patom_rs::runtime::PromptRequestId;
use patom_rs::session::{PgSessionStore, SessionStore};
use patom_rs::types::Participant;

mod common;
use common::pg::TestDb;

fn store(db: &TestDb) -> Arc<PgSessionStore> {
    Arc::new(PgSessionStore::new(db.pool.clone(), SystemClock::shared()))
}

/// Create a real agent row so FK-bearing inserts (sessions / session_messages)
/// can reference it. Returns the [`Participant::Agent`] wrapping its id.
async fn fresh_agent(db: &TestDb, name: &str) -> Participant {
    let store: SharedAgentStore =
        common::pg::shared_agent_store(db.pool.clone(), SystemClock::shared());
    let record = store
        .create(NewAgent {
            org_id: db.default_org_id,
            name: AgentName::try_from(name).expect("name"),
            system_prompt: AgentSystemPrompt::try_from("test prompt").expect("prompt"),
            description: patom_rs::agents::AgentDescription::try_from("test desc").expect("desc"),
            is_default: false,
            allowed_mcp_tools: patom_rs::agents::AllowedMcpTools::empty(),
            model: None,
            edited_by: None,
        })
        .await
        .expect("create agent");
    Participant::agent(record.id)
}

#[tokio::test(flavor = "multi_thread")]
async fn same_pair_same_dag_returns_same_session() {
    let db = TestDb::fresh().await;
    let store = store(&db);
    let root = PromptRequestId::new();
    let a = Participant::Human;
    let b = Participant::agent(db.default_agent_id);

    let first = store
        .resolve_or_create_for_pair(root, a, b, None, db.default_org_id, db.default_user_id)
        .await
        .expect("first");
    let second = store
        .resolve_or_create_for_pair(root, a, b, None, db.default_org_id, db.default_user_id)
        .await
        .expect("second");
    assert_eq!(first, second, "upsert is idempotent on same key");
}

#[tokio::test(flavor = "multi_thread")]
async fn reversed_pair_canonicalises_to_same_session() {
    // Caller may pass `(a, b)` either way round; the store canonicalises so
    // both orderings hit the same `sessions_dag_pair_unique` index entry.
    let db = TestDb::fresh().await;
    let store = store(&db);
    let root = PromptRequestId::new();
    let h = Participant::Human;
    let a = Participant::agent(db.default_agent_id);

    let forward = store
        .resolve_or_create_for_pair(root, h, a, None, db.default_org_id, db.default_user_id)
        .await
        .expect("forward");
    let reversed = store
        .resolve_or_create_for_pair(root, a, h, None, db.default_org_id, db.default_user_id)
        .await
        .expect("reversed");
    assert_eq!(forward, reversed);
}

#[tokio::test(flavor = "multi_thread")]
async fn different_dags_get_distinct_sessions() {
    let db = TestDb::fresh().await;
    let store = store(&db);
    let pair = (Participant::Human, Participant::agent(db.default_agent_id));

    let dag_a = PromptRequestId::new();
    let dag_b = PromptRequestId::new();
    let s_a = store
        .resolve_or_create_for_pair(
            dag_a,
            pair.0,
            pair.1,
            None,
            db.default_org_id,
            db.default_user_id,
        )
        .await
        .expect("dag_a");
    let s_b = store
        .resolve_or_create_for_pair(
            dag_b,
            pair.0,
            pair.1,
            None,
            db.default_org_id,
            db.default_user_id,
        )
        .await
        .expect("dag_b");
    assert_ne!(s_a, s_b, "DAG isolation: same pair, different roots");
}

#[tokio::test(flavor = "multi_thread")]
async fn parent_session_is_recorded() {
    // Forked sessions (e.g. agent A spawns conversation with agent B) carry
    // their parent so the agent loop can auto-load it on the receiver's
    // first turn.
    let db = TestDb::fresh().await;
    let store = store(&db);
    let root = PromptRequestId::new();
    let agent_a = Participant::agent(db.default_agent_id);
    let agent_b = fresh_agent(&db, "second").await;

    let parent_id = store
        .resolve_or_create_for_pair(
            root,
            Participant::Human,
            agent_a,
            None,
            db.default_org_id,
            db.default_user_id,
        )
        .await
        .expect("parent");
    let child_id = store
        .resolve_or_create_for_pair(
            root,
            agent_a,
            agent_b,
            Some(parent_id),
            db.default_org_id,
            db.default_user_id,
        )
        .await
        .expect("child");

    let recovered = store.parent(child_id).await.expect("parent lookup");
    assert_eq!(recovered, Some(parent_id));
    let root_parent = store.parent(parent_id).await.expect("root lookup");
    assert_eq!(root_parent, None, "root session has no parent");
}

#[tokio::test(flavor = "multi_thread")]
async fn participants_are_returned_in_canonical_order() {
    // Agent < Human by canonical_cmp (matches SQL string-compare on the
    // *_kind columns), so participants() returns (Agent(_), Human)
    // regardless of which side the caller passed first.
    let db = TestDb::fresh().await;
    let store = store(&db);
    let root = PromptRequestId::new();
    let agent_p = Participant::agent(db.default_agent_id);

    let id = store
        .resolve_or_create_for_pair(
            root,
            Participant::Human,
            agent_p,
            None,
            db.default_org_id,
            db.default_user_id,
        )
        .await
        .expect("session");
    let (a, b) = store.participants(id).await.expect("participants");
    assert_eq!(a, agent_p);
    assert_eq!(b, Participant::Human);
}

#[tokio::test(flavor = "multi_thread")]
async fn root_request_id_round_trips() {
    let db = TestDb::fresh().await;
    let store = store(&db);
    let root = PromptRequestId::new();
    let id = store
        .resolve_or_create_for_pair(
            root,
            Participant::Human,
            Participant::agent(db.default_agent_id),
            None,
            db.default_org_id,
            db.default_user_id,
        )
        .await
        .expect("session");
    let resolved = store.root_request_id(id).await.expect("root");
    assert_eq!(resolved, root);
}

#[tokio::test(flavor = "multi_thread")]
async fn self_session_is_rejected() {
    let db = TestDb::fresh().await;
    let store = store(&db);
    let root = PromptRequestId::new();
    let err = store
        .resolve_or_create_for_pair(
            root,
            Participant::agent(db.default_agent_id),
            Participant::agent(db.default_agent_id),
            None,
            db.default_org_id,
            db.default_user_id,
        )
        .await
        .expect_err("self-session forbidden");
    assert!(matches!(err, patom_rs::session::SessionError::SelfSession));
}
