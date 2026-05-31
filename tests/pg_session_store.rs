//! Trait-contract tests for [`patom_rs::session::PgSessionStore`]. Each test
//! gets its own freshly-migrated database via `#[sqlx::test]`, so they run in
//! parallel with full isolation.

#![allow(clippy::expect_used)]

use std::sync::Arc;

use patom_rs::agents::AgentId;
use patom_rs::clock::SystemClock;
use patom_rs::provider::{ChatMessage, UserContent};
use patom_rs::runtime::PromptRequestId;
use patom_rs::session::{PgSessionStore, SessionError, SessionId, SessionStore};
use patom_rs::types::{MessageSender, Participant};
use sqlx::PgPool;

mod common;
use common::pg::{human_to_agent_session, seed_prompt_request, seed_tenant};

fn store(pool: &PgPool) -> Arc<PgSessionStore> {
    Arc::new(PgSessionStore::new(pool.clone(), SystemClock::shared()))
}

#[sqlx::test]
async fn create_append_snapshot_roundtrip(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let store = store(&pool);

    let agent = Participant::agent(seed.agent_id);
    let id = human_to_agent_session(store.as_ref(), seed.agent_id, seed.org_id, seed.user_id).await;
    let req = seed_prompt_request(&pool, id, seed.agent_id, seed.org_id).await;
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

#[sqlx::test]
async fn missing_session_is_not_found(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let store = store(&pool);

    let id = SessionId::new();
    let viewer = Participant::agent(seed.agent_id);
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

#[sqlx::test]
async fn enforces_message_cap(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let store = Arc::new(PgSessionStore::with_caps(
        pool.clone(),
        SystemClock::shared(),
        2,
    ));
    let id = human_to_agent_session(store.as_ref(), seed.agent_id, seed.org_id, seed.user_id).await;
    let req = seed_prompt_request(&pool, id, seed.agent_id, seed.org_id).await;
    let agent = Participant::agent(seed.agent_id);
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

#[sqlx::test]
async fn delete_cascades_messages(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let store = store(&pool);
    let id = human_to_agent_session(store.as_ref(), seed.agent_id, seed.org_id, seed.user_id).await;
    let req = seed_prompt_request(&pool, id, seed.agent_id, seed.org_id).await;
    let agent = Participant::agent(seed.agent_id);
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

#[sqlx::test]
async fn create_with_unknown_agent_returns_agent_not_found(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let store = store(&pool);

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
            seed.org_id,
            seed.user_id,
        )
        .await
        .expect_err("fk");
    assert!(matches!(err, SessionError::AgentNotFound(_)));
}

#[sqlx::test]
async fn participants_round_trip_through_session(pool: PgPool) {
    // Canonical order is Agent < Human (matches SQL CHECK).
    let seed = seed_tenant(&pool).await;
    let store = store(&pool);

    let id = human_to_agent_session(store.as_ref(), seed.agent_id, seed.org_id, seed.user_id).await;
    let (a, b) = store.participants(id).await.expect("resolve");
    assert_eq!(a, Participant::agent(seed.agent_id));
    assert_eq!(b, Participant::Human);
}

#[sqlx::test]
async fn snapshot_renders_messages_from_viewer_perspective(pool: PgPool) {
    // sender == viewer => Assistant; otherwise => User. This is the central
    // contract of the new viewer-mapped snapshot.
    let seed = seed_tenant(&pool).await;
    let store = store(&pool);
    let agent = Participant::agent(seed.agent_id);

    let id = human_to_agent_session(store.as_ref(), seed.agent_id, seed.org_id, seed.user_id).await;
    let req = seed_prompt_request(&pool, id, seed.agent_id, seed.org_id).await;
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
