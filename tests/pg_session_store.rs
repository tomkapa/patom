//! Trait-contract tests for [`patom_rs::session::PgSessionStore`]. Each test owns its
//! own schema via `TestDb::fresh` so they can run in parallel.

#![allow(clippy::expect_used)]

use std::sync::Arc;

use patom_rs::agents::AgentId;
use patom_rs::clock::SystemClock;
use patom_rs::provider::{ChatMessage, UserContent};
use patom_rs::runtime::PromptRequestId;
use patom_rs::session::{PgSessionStore, SessionError, SessionId, SessionStore};
use patom_rs::types::{MessageSender, Participant};

mod common;
use common::pg::{TestDb, human_to_agent_session, seed_prompt_request};

fn store(db: &TestDb) -> Arc<PgSessionStore> {
    Arc::new(PgSessionStore::new(db.pool.clone(), SystemClock::shared()))
}

#[tokio::test(flavor = "multi_thread")]
async fn create_append_snapshot_roundtrip() {
    let db = TestDb::fresh().await;
    let store = store(&db);

    let agent = Participant::agent(db.default_agent_id);
    let id = human_to_agent_session(
        store.as_ref(),
        db.default_agent_id,
        db.default_org_id,
        db.default_user_id,
    )
    .await;
    let req = seed_prompt_request(&db.pool, id, db.default_agent_id, db.default_org_id).await;
    store
        .append(
            id,
            MessageSender::Human,
            agent,
            ChatMessage::User(vec![UserContent::Text("hi".into())]),
            req,
        )
        .await
        .expect("append");
    store
        .append(
            id,
            MessageSender::Human,
            agent,
            ChatMessage::User(vec![UserContent::Text("again".into())]),
            req,
        )
        .await
        .expect("append2");

    // Viewer = the agent. Both rows came from human → render as User to the
    // agent.
    let snap = store.snapshot(id, agent).await.expect("snapshot");
    assert_eq!(snap.len(), 2);
    let ChatMessage::User(contents) = &snap[0] else {
        panic!("first message should be user");
    };
    assert!(matches!(&contents[0], UserContent::Text(t) if t == "hi"));
}

#[tokio::test(flavor = "multi_thread")]
async fn missing_session_is_not_found() {
    let db = TestDb::fresh().await;
    let store = store(&db);

    let id = SessionId::new();
    let viewer = Participant::agent(db.default_agent_id);
    let err = store.snapshot(id, viewer).await.expect_err("absent");
    assert!(matches!(err, SessionError::NotFound(_)));

    let err = store
        .append(
            id,
            MessageSender::Human,
            viewer,
            ChatMessage::User(vec![UserContent::Text("hi".into())]),
            PromptRequestId::new(),
        )
        .await
        .expect_err("absent append");
    assert!(matches!(err, SessionError::NotFound(_)));

    let err = store.delete(id).await.expect_err("absent delete");
    assert!(matches!(err, SessionError::NotFound(_)));
}

#[tokio::test(flavor = "multi_thread")]
async fn enforces_message_cap() {
    let db = TestDb::fresh().await;
    let store = Arc::new(PgSessionStore::with_caps(
        db.pool.clone(),
        SystemClock::shared(),
        2,
    ));
    let id = human_to_agent_session(
        store.as_ref(),
        db.default_agent_id,
        db.default_org_id,
        db.default_user_id,
    )
    .await;
    let req = seed_prompt_request(&db.pool, id, db.default_agent_id, db.default_org_id).await;
    let agent = Participant::agent(db.default_agent_id);
    for _ in 0..2 {
        store
            .append(
                id,
                MessageSender::Human,
                agent,
                ChatMessage::User(vec![UserContent::Text("x".into())]),
                req,
            )
            .await
            .expect("under cap");
    }
    let err = store
        .append(
            id,
            MessageSender::Human,
            agent,
            ChatMessage::User(vec![UserContent::Text("over".into())]),
            req,
        )
        .await
        .expect_err("at cap");
    assert!(matches!(err, SessionError::MessageCapExceeded { .. }));
}

#[tokio::test(flavor = "multi_thread")]
async fn delete_cascades_messages() {
    let db = TestDb::fresh().await;
    let store = store(&db);
    let id = human_to_agent_session(
        store.as_ref(),
        db.default_agent_id,
        db.default_org_id,
        db.default_user_id,
    )
    .await;
    let req = seed_prompt_request(&db.pool, id, db.default_agent_id, db.default_org_id).await;
    let agent = Participant::agent(db.default_agent_id);
    store
        .append(
            id,
            MessageSender::Human,
            agent,
            ChatMessage::User(vec![UserContent::Text("hi".into())]),
            req,
        )
        .await
        .expect("append");

    store.delete(id).await.expect("delete");
    let err = store.snapshot(id, agent).await.expect_err("gone");
    assert!(matches!(err, SessionError::NotFound(_)));
}

#[tokio::test(flavor = "multi_thread")]
async fn create_with_unknown_agent_returns_agent_not_found() {
    let db = TestDb::fresh().await;
    let store = store(&db);

    // Random uuid that does not exist in the agents table — the FK on
    // participant_*_agent_id should reject the insert and surface as
    // SessionError::AgentNotFound.
    let phantom = AgentId::new();
    let root = PromptRequestId::new();
    let err = store
        .resolve_or_create_for_pair(
            root,
            Participant::Human,
            Participant::agent(phantom),
            None,
            db.default_org_id,
            db.default_user_id,
        )
        .await
        .expect_err("fk");
    assert!(matches!(err, SessionError::AgentNotFound(_)));
}

#[tokio::test(flavor = "multi_thread")]
async fn participants_round_trip_through_session() {
    // Canonical order is Agent < Human (matches SQL CHECK).
    let db = TestDb::fresh().await;
    let store = store(&db);

    let id = human_to_agent_session(
        store.as_ref(),
        db.default_agent_id,
        db.default_org_id,
        db.default_user_id,
    )
    .await;
    let (a, b) = store.participants(id).await.expect("resolve");
    assert_eq!(a, Participant::agent(db.default_agent_id));
    assert_eq!(b, Participant::Human);
}

#[tokio::test(flavor = "multi_thread")]
async fn snapshot_renders_messages_from_viewer_perspective() {
    // sender == viewer => Assistant; otherwise => User. This is the central
    // contract of the new viewer-mapped snapshot.
    let db = TestDb::fresh().await;
    let store = store(&db);
    let agent = Participant::agent(db.default_agent_id);

    let id = human_to_agent_session(
        store.as_ref(),
        db.default_agent_id,
        db.default_org_id,
        db.default_user_id,
    )
    .await;
    let req = seed_prompt_request(&db.pool, id, db.default_agent_id, db.default_org_id).await;
    // Human → Agent: text prompt.
    store
        .append(
            id,
            MessageSender::Human,
            agent,
            ChatMessage::User(vec![UserContent::Text("ping".into())]),
            req,
        )
        .await
        .expect("append");
    // Agent → Human: assistant text.
    store
        .append(
            id,
            MessageSender::from_participant(agent),
            Participant::Human,
            ChatMessage::Assistant(vec![patom_rs::provider::AssistantContent::Text(
                "pong".into(),
            )]),
            req,
        )
        .await
        .expect("append");

    // Viewer = agent: the human's row is User, agent's row is Assistant.
    let snap = store.snapshot(id, agent).await.expect("agent view");
    assert!(matches!(&snap[0], ChatMessage::User(_)));
    assert!(matches!(&snap[1], ChatMessage::Assistant(_)));

    // Viewer = human: the human's row is Assistant, agent's row is User.
    let snap = store
        .snapshot(id, Participant::Human)
        .await
        .expect("human view");
    assert!(matches!(&snap[0], ChatMessage::Assistant(_)));
    assert!(matches!(&snap[1], ChatMessage::User(_)));
}
