//! P8 (foundation): the background-cognition store keeps reflection/resolution
//! turns OFF the chat feed.
//!
//! A background turn's LLM exchange is appended to `background_turn_messages`
//! and replayed via `context`; crucially it lands no `thread_messages` row, so
//! cognition never pollutes the chat. The end-to-end
//! `reflection_writes_no_thread_message_rows` (worker-driven) builds on this.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use patom::auth::Caller;
use patom::background::{BackgroundStore, NewBackgroundMessage, PgBackgroundStore};
use patom::clock::SystemClock;
use patom::colleagues::resolve_agent_colleague;
use patom::provider::{AssistantContent, ChatMessage, UserContent};
use sqlx::PgPool;

mod common;
use common::pg::seed_tenant;

#[sqlx::test]
async fn background_turn_log_roundtrips_off_the_chat_feed(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let store = PgBackgroundStore::new(pool.clone(), SystemClock::shared());
    let caller = Caller::new(seed.user_id, seed.org_id);
    let agent_col = resolve_agent_colleague(&pool, seed.org_id, seed.agent_id)
        .await
        .expect("agent colleague");

    let turn = store
        .create_turn(&caller, seed.agent_id)
        .await
        .expect("create background turn");

    // System-injected reflection prompt, then the agent's reply.
    store
        .append(
            &caller,
            turn,
            NewBackgroundMessage {
                sender: None,
                body: ChatMessage::User(vec![UserContent::Text("reflect on the last hour".into())]),
                request_id: None,
            },
        )
        .await
        .expect("append prompt");
    store
        .append(
            &caller,
            turn,
            NewBackgroundMessage {
                sender: Some(agent_col),
                body: ChatMessage::Assistant(vec![AssistantContent::Text(
                    "noted a pattern".into(),
                )]),
                request_id: None,
            },
        )
        .await
        .expect("append reply");

    // Context replays both rows in seq order.
    let ctx = store.context(&caller, turn).await.expect("context");
    assert_eq!(ctx.len(), 2, "both background messages replay");
    assert!(matches!(&ctx[0], ChatMessage::User(_)), "prompt first");
    assert!(matches!(&ctx[1], ChatMessage::Assistant(_)), "reply second");

    // The headline invariant: nothing leaked into the chat feed.
    let (thread_rows,): (i64,) = sqlx::query_as("SELECT count(*) FROM thread_messages")
        .fetch_one(&pool)
        .await
        .expect("count thread_messages");
    assert_eq!(
        thread_rows, 0,
        "background cognition must write no chat-feed rows"
    );
}
