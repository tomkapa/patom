//! Trait-contract tests for [`patom::session::PgSessionStore`]. Each test
//! gets its own freshly-migrated database via `#[sqlx::test]`, so they run in
//! parallel with full isolation.

#![allow(clippy::expect_used)]

use std::sync::Arc;

use patom::agents::AgentId;
use patom::clock::SystemClock;
use patom::provider::{ChatMessage, UserContent};
use patom::runtime::PromptRequestId;
use patom::session::{PgSessionStore, SessionError, SessionId, SessionStore};
use patom::types::MessageSender;
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

    let agent = common::pg::agent_participant(&pool, seed.org_id, seed.agent_id).await;
    let id = human_to_agent_session(
        &pool,
        store.as_ref(),
        seed.agent_id,
        seed.org_id,
        seed.user_id,
    )
    .await;
    let req = seed_prompt_request(&pool, id, seed.agent_id, seed.org_id).await;
    store
        .append(
            id,
            common::pg::human_sender(&pool, seed.org_id, seed.user_id).await,
            agent,
            ChatMessage::User(vec![UserContent::Text("hi".into())]),
            req,
        )
        .await
        .expect("append");
    store
        .append(
            id,
            common::pg::human_sender(&pool, seed.org_id, seed.user_id).await,
            agent,
            ChatMessage::User(vec![UserContent::Text("again".into())]),
            req,
        )
        .await
        .expect("append2");

    // Viewer = the agent. Both rows came from human → render as User to the
    // agent.
    let snap = store
        .snapshot(id, agent.colleague_id().expect("real colleague"))
        .await
        .expect("snapshot");
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
    let viewer = common::pg::agent_participant(&pool, seed.org_id, seed.agent_id).await;
    let err = store
        .snapshot(id, viewer.colleague_id().expect("real colleague"))
        .await
        .expect_err("absent");
    assert!(matches!(err, SessionError::NotFound(_)));

    let err = store
        .append(
            id,
            common::pg::human_sender(&pool, seed.org_id, seed.user_id).await,
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
    let id = human_to_agent_session(
        &pool,
        store.as_ref(),
        seed.agent_id,
        seed.org_id,
        seed.user_id,
    )
    .await;
    let req = seed_prompt_request(&pool, id, seed.agent_id, seed.org_id).await;
    let agent = common::pg::agent_participant(&pool, seed.org_id, seed.agent_id).await;
    for _ in 0..2 {
        store
            .append(
                id,
                common::pg::human_sender(&pool, seed.org_id, seed.user_id).await,
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
            common::pg::human_sender(&pool, seed.org_id, seed.user_id).await,
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
    let id = human_to_agent_session(
        &pool,
        store.as_ref(),
        seed.agent_id,
        seed.org_id,
        seed.user_id,
    )
    .await;
    let req = seed_prompt_request(&pool, id, seed.agent_id, seed.org_id).await;
    let agent = common::pg::agent_participant(&pool, seed.org_id, seed.agent_id).await;
    store
        .append(
            id,
            common::pg::human_sender(&pool, seed.org_id, seed.user_id).await,
            agent,
            ChatMessage::User(vec![UserContent::Text("hi".into())]),
            req,
        )
        .await
        .expect("append");

    store.delete(id).await.expect("delete");
    let err = store
        .snapshot(id, agent.colleague_id().expect("real colleague"))
        .await
        .expect_err("gone");
    assert!(matches!(err, SessionError::NotFound(_)));
}

#[sqlx::test]
async fn create_with_unknown_agent_returns_colleague_not_found(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let store = store(&pool);

    // Random colleague id that does not exist in the colleagues table — the
    // FK on participant_*_colleague_id should reject the insert and surface
    // as SessionError::ColleagueNotFound. We construct a synthetic agent
    // Participant with an unmapped colleague id rather than going through
    // `agent_participant`, which assumes the colleague trigger has fired.
    let phantom_colleague = patom::colleagues::ColleagueId::new();
    let phantom_agent = AgentId::new();
    let agent = patom::types::Participant::agent(phantom_colleague, phantom_agent);
    let root = PromptRequestId::new();
    let err = store
        .resolve_or_create_for_pair(
            root,
            common::pg::human_participant(&pool, seed.org_id, seed.user_id).await,
            agent,
            None,
            seed.org_id,
            seed.user_id,
        )
        .await
        .expect_err("fk");
    assert!(matches!(err, SessionError::ColleagueNotFound(_)));
}

#[sqlx::test]
async fn participants_round_trip_through_session(pool: PgPool) {
    // Canonical order (Stage 3a): real colleagues sort by `colleague_id` UUID.
    // Whichever participant has the lower UUID lands in slot `a`; the test
    // asserts the pair contains both ends and uses `canonical_pair` to
    // canonicalise so the ordering is deterministic for the assertion.
    let seed = seed_tenant(&pool).await;
    let store = store(&pool);

    let id = human_to_agent_session(
        &pool,
        store.as_ref(),
        seed.agent_id,
        seed.org_id,
        seed.user_id,
    )
    .await;
    let (a, b) = store.participants(id).await.expect("resolve");
    let human = common::pg::human_participant(&pool, seed.org_id, seed.user_id).await;
    let agent = common::pg::agent_participant(&pool, seed.org_id, seed.agent_id).await;
    let (canonical_a, canonical_b) =
        patom::types::Participant::canonical_pair(human, agent).expect("distinct");
    assert_eq!(a, canonical_a);
    assert_eq!(b, canonical_b);
}

#[sqlx::test]
async fn snapshot_renders_messages_from_viewer_perspective(pool: PgPool) {
    // sender == viewer => Assistant; otherwise => User. This is the central
    // contract of the new viewer-mapped snapshot.
    let seed = seed_tenant(&pool).await;
    let store = store(&pool);
    let agent = common::pg::agent_participant(&pool, seed.org_id, seed.agent_id).await;

    let id = human_to_agent_session(
        &pool,
        store.as_ref(),
        seed.agent_id,
        seed.org_id,
        seed.user_id,
    )
    .await;
    let req = seed_prompt_request(&pool, id, seed.agent_id, seed.org_id).await;
    // Human → Agent: text prompt.
    store
        .append(
            id,
            common::pg::human_sender(&pool, seed.org_id, seed.user_id).await,
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
            common::pg::human_participant(&pool, seed.org_id, seed.user_id).await,
            ChatMessage::Assistant(vec![patom::provider::AssistantContent::Text("pong".into())]),
            req,
        )
        .await
        .expect("append");

    // Viewer = agent: the human's row is User, agent's row is Assistant.
    let snap = store
        .snapshot(id, agent.colleague_id().expect("real colleague"))
        .await
        .expect("agent view");
    assert!(matches!(&snap[0], ChatMessage::User(_)));
    assert!(matches!(&snap[1], ChatMessage::Assistant(_)));

    // Viewer = human: the human's row is Assistant, agent's row is User.
    let snap = store
        .snapshot(
            id,
            patom::colleagues::resolve_user_colleague(&pool, seed.org_id, seed.user_id)
                .await
                .expect("colleague"),
        )
        .await
        .expect("human view");
    assert!(matches!(&snap[0], ChatMessage::Assistant(_)));
    assert!(matches!(&snap[1], ChatMessage::User(_)));
}
