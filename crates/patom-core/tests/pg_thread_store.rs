//! P1 contract test for [`PgThreadStore`]: an agent's context is the posted
//! feed (from anyone) plus its OWN private artifacts — never a peer's.

mod common;

use patom::agents::AgentId;
use patom::auth::Caller;
use patom::clock::SystemClock;
use patom::colleagues::{resolve_agent_colleague, resolve_user_colleague};
use patom::provider::{
    AssistantContent, ChatMessage, ToolCall, ToolCallId, ToolResult, UserContent,
};
use patom::threads::{MessageKind, NewMessage, PgThreadStore, ThreadStore};
use patom::types::ToolName;
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
        idempotency_key: None,
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
        idempotency_key: None,
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

fn tool_use(owner: AgentId, call_id: &str) -> NewMessage {
    NewMessage {
        kind: MessageKind::ToolUse,
        sender: None,
        owner_agent_id: Some(owner),
        receiver: None,
        body: ChatMessage::Assistant(vec![AssistantContent::ToolCall(ToolCall {
            id: ToolCallId::try_from(call_id).expect("call id"),
            name: ToolName::try_from("web_search").expect("tool name"),
            input: serde_json::json!({ "q": "x" }),
        })]),
        request_id: None,
        idempotency_key: None,
    }
}

fn tool_result(owner: AgentId, call_id: &str) -> NewMessage {
    NewMessage {
        kind: MessageKind::ToolResult,
        sender: None,
        owner_agent_id: Some(owner),
        receiver: None,
        body: ChatMessage::User(vec![UserContent::ToolResult(ToolResult {
            call_id: ToolCallId::try_from(call_id).expect("call id"),
            output: "search results".into(),
            is_error: false,
        })]),
        request_id: None,
        idempotency_key: None,
    }
}

/// note 13: a peer's posted row can land between an agent's tool_use and its
/// tool_result by `seq` (threads are multi-writer). `context_for_agent` must
/// re-pair them so the provider sees the tool_result immediately after the
/// tool_use — never split by the interleaved post.
#[sqlx::test]
async fn context_repairs_tool_use_result_split_by_peer_post(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let store = PgThreadStore::new(pool.clone(), SystemClock::shared());
    let caller = Caller::new(seed.user_id, seed.org_id);

    let agent_a = seed.agent_id;
    let col_a = resolve_agent_colleague(&pool, seed.org_id, agent_a)
        .await
        .expect("colleague a");
    let col_h = resolve_user_colleague(&pool, seed.org_id, seed.user_id)
        .await
        .expect("human colleague");

    let thread = store
        .create_thread(&caller, None, None, col_h, Some(col_a))
        .await
        .expect("create thread");
    store
        .resolve_participation(&caller, thread, agent_a)
        .await
        .expect("participation a");

    // A emits a tool_use; a human posts before A's tool_result lands; then the
    // tool_result arrives. By `seq`: tool_use < posted < tool_result.
    for m in [
        tool_use(agent_a, "call-1"),
        posted(
            col_h,
            Some(col_a),
            "interrupting while the tool runs",
            false,
        ),
        tool_result(agent_a, "call-1"),
    ] {
        store.append(&caller, thread, m).await.expect("append");
    }

    let ctx = store
        .context_for_agent(thread, agent_a, col_a, &std::collections::HashMap::new())
        .await
        .expect("ctx");
    assert_eq!(ctx.len(), 3);

    // The tool_use (Assistant w/ ToolCall) is immediately followed by its
    // tool_result (User w/ ToolResult); the peer post is deferred to after.
    let tool_use_idx = ctx
        .iter()
        .position(|m| {
            matches!(m, ChatMessage::Assistant(b)
                if b.iter().any(|x| matches!(x, AssistantContent::ToolCall(_))))
        })
        .expect("tool_use present");
    assert!(
        matches!(&ctx[tool_use_idx + 1], ChatMessage::User(b)
            if b.iter().any(|x| matches!(x, UserContent::ToolResult(_)))),
        "tool_result must immediately follow tool_use, got {:?}",
        ctx[tool_use_idx + 1],
    );
    // The interleaving peer post lands after the pair, not between it.
    assert!(
        user_text_present(&ctx, "interrupting while the tool runs"),
        "peer post is preserved (deferred after the pair)"
    );
    assert_eq!(
        tool_use_idx, 0,
        "tool_use stays first; post moved after pair"
    );
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
        "INSERT INTO agents (id, name, created_at, updated_at, description, org_id) \
         VALUES ($1, $2, now(), now(), $3, $4)",
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
        .create_thread(&caller, None, None, col_h, Some(col_b))
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
    // Every production writer (`send_message`, the HTTP prompt route, the Slack
    // bridge) stores a `posted` body in the neutral `User` perspective — so A's
    // OWN post is seeded as `User` here too. `context_for_agent` must re-tag it
    // to `Assistant` for A, or A re-reads its own send_message output as a user
    // turn and starts replying to itself.
    for m in [
        posted(col_h, Some(col_a), "hello", false),
        posted(col_a, None, "hi from A", false),
        reasoning(col_a, agent_a, "A thinking"),
        reasoning(col_b, agent_b, "B thinking"),
    ] {
        store.append(&caller, thread, m).await.expect("append");
    }

    let ctx_a = store
        .context_for_agent(thread, agent_a, col_a, &std::collections::HashMap::new())
        .await
        .expect("ctx a");
    let ctx_b = store
        .context_for_agent(thread, agent_b, col_b, &std::collections::HashMap::new())
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
    // The peer's post is attributed by name so the agent can tell speakers
    // apart in a multi-party thread (canonical name; no platform override here).
    assert!(
        user_text_present(&ctx_a, "Seeded Test User: "),
        "human post is prefixed with the sender's name"
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
