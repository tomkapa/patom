//! P1 contract test for [`PgThreadStore`]: an agent's context is the posted
//! feed (from anyone) plus its OWN private artifacts — never a peer's.

mod common;

use patom::agents::AgentId;
use patom::auth::Caller;
use patom::clock::SystemClock;
use patom::colleagues::{resolve_agent_colleague, resolve_user_colleague};
use patom::provider::{AssistantContent, ChatMessage, UserContent};
use patom::threads::{MessageKind, NewMessage, PgThreadStore, ThreadStore};
use sqlx::PgPool;

use common::pg::seed_tenant;

fn posted(
    sender: patom::colleagues::ColleagueId,
    receiver: Option<patom::colleagues::ColleagueId>,
    text: &str,
    assistant: bool,
) -> NewMessage {
    let body = if assistant {
        ChatMessage::Assistant(vec![AssistantContent::Text(text.into())])
    } else {
        ChatMessage::User(vec![UserContent::Text(text.into())])
    };
    NewMessage {
        kind: MessageKind::Posted,
        sender: Some(sender),
        owner_agent_id: None,
        receiver,
        body,
        request_id: None,
    }
}

fn reasoning(sender: patom::colleagues::ColleagueId, owner: AgentId, text: &str) -> NewMessage {
    NewMessage {
        kind: MessageKind::Reasoning,
        sender: Some(sender),
        owner_agent_id: Some(owner),
        receiver: None,
        body: ChatMessage::Assistant(vec![AssistantContent::Reasoning(text.into())]),
        request_id: None,
    }
}

fn has_reasoning(ctx: &[ChatMessage], needle: &str) -> bool {
    ctx.iter().any(|m| {
        matches!(m, ChatMessage::Assistant(blocks)
            if blocks.iter().any(|b| matches!(b, AssistantContent::Reasoning(t) if t == needle)))
    })
}

fn assistant_text_present(ctx: &[ChatMessage], needle: &str) -> bool {
    ctx.iter().any(|m| {
        matches!(m, ChatMessage::Assistant(blocks)
            if blocks.iter().any(|b| matches!(b, AssistantContent::Text(t) if t == needle)))
    })
}

fn user_text_present(ctx: &[ChatMessage], needle: &str) -> bool {
    ctx.iter().any(|m| {
        matches!(m, ChatMessage::User(blocks)
            if blocks.iter().any(|b| matches!(b, UserContent::Text(t) if t == needle)))
    })
}

#[sqlx::test]
async fn context_filters_private_rows_by_owner(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let store = PgThreadStore::new(pool.clone(), SystemClock::shared());
    let caller = Caller::new(seed.user_id, seed.org_id);

    // Agent A is the seeded default; create Agent B (the trigger mints its colleague).
    let agent_a = seed.agent_id;
    let agent_b = AgentId::new();
    sqlx::query(
        "INSERT INTO agents (id, name, is_default, created_at, updated_at, description, org_id) \
         VALUES ($1, $2, false, now(), now(), $3, $4)",
    )
    .bind(agent_b)
    .bind("agent-b")
    .bind("Agent B")
    .bind(seed.org_id)
    .execute(&pool)
    .await
    .expect("insert agent b");

    let col_a = resolve_agent_colleague(&pool, seed.org_id, agent_a)
        .await
        .expect("colleague a");
    let col_b = resolve_agent_colleague(&pool, seed.org_id, agent_b)
        .await
        .expect("colleague b");
    let col_h = resolve_user_colleague(&pool, seed.org_id, seed.user_id)
        .await
        .expect("human colleague");

    let thread = store
        .create_thread(&caller, None, None, col_h)
        .await
        .expect("create thread");
    store
        .resolve_participation(&caller, thread, agent_a)
        .await
        .expect("participation a");
    store
        .resolve_participation(&caller, thread, agent_b)
        .await
        .expect("participation b");

    // Feed: human post, A's post, A's private reasoning, B's private reasoning.
    for m in [
        posted(col_h, Some(col_a), "hello", false),
        posted(col_a, None, "hi from A", true),
        reasoning(col_a, agent_a, "A thinking"),
        reasoning(col_b, agent_b, "B thinking"),
    ] {
        store.append(&caller, thread, m).await.expect("append");
    }

    let ctx_a = store
        .context_for_agent(thread, agent_a, col_a)
        .await
        .expect("ctx a");
    let ctx_b = store
        .context_for_agent(thread, agent_b, col_b)
        .await
        .expect("ctx b");

    // A: human post + A's post + A's reasoning = 3; B's reasoning excluded.
    assert_eq!(ctx_a.len(), 3, "A sees posted ∪ own-private");
    assert!(
        has_reasoning(&ctx_a, "A thinking"),
        "A sees its own reasoning"
    );
    assert!(
        !has_reasoning(&ctx_a, "B thinking"),
        "A must NOT see B's reasoning"
    );
    assert!(
        assistant_text_present(&ctx_a, "hi from A"),
        "A's own post maps to Assistant"
    );
    assert!(
        user_text_present(&ctx_a, "hello"),
        "human post maps to User"
    );

    // B: human post + A's post (as User) + B's reasoning = 3; A's reasoning excluded.
    assert_eq!(ctx_b.len(), 3, "B sees posted ∪ own-private");
    assert!(
        has_reasoning(&ctx_b, "B thinking"),
        "B sees its own reasoning"
    );
    assert!(
        !has_reasoning(&ctx_b, "A thinking"),
        "B must NOT see A's reasoning"
    );
    assert!(
        user_text_present(&ctx_b, "hi from A"),
        "A's post maps to User for B"
    );
}
