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
use patom::threads::{
    MAX_CONTEXT_MESSAGES, MAX_TOOL_RESULT_CHARS, MessageKind, NewMessage, PgThreadStore, Seq,
    ThreadStore,
};
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

fn first_tool_result_output(ctx: &[ChatMessage]) -> Option<String> {
    for m in ctx {
        if let ChatMessage::User(blocks) = m {
            for b in blocks {
                if let UserContent::ToolResult(r) = b {
                    return Some(r.output.clone());
                }
            }
        }
    }
    None
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
    tool_result_with(owner, call_id, "search results")
}

fn tool_result_with(owner: AgentId, call_id: &str, output: &str) -> NewMessage {
    NewMessage {
        kind: MessageKind::ToolResult,
        sender: None,
        owner_agent_id: Some(owner),
        receiver: None,
        body: ChatMessage::User(vec![UserContent::ToolResult(ToolResult {
            call_id: ToolCallId::try_from(call_id).expect("call id"),
            output: output.into(),
            is_error: false,
        })]),
        request_id: None,
        idempotency_key: None,
    }
}

/// The per-thread feed seqs of `thread`, ascending.
async fn thread_seqs(pool: &PgPool, thread: patom::threads::ThreadId) -> Vec<Seq> {
    let raw: Vec<i64> =
        sqlx::query_scalar("SELECT seq FROM thread_messages WHERE thread_id = $1 ORDER BY seq ASC")
            .bind(thread)
            .fetch_all(pool)
            .await
            .expect("seqs");
    raw.into_iter()
        .map(|v| Seq::try_from(v).expect("valid seq"))
        .collect()
}

/// Position of the first message whose text contains `needle`, or `None`.
fn user_text_index(ctx: &[ChatMessage], needle: &str) -> Option<usize> {
    ctx.iter().position(|m| {
        matches!(m, ChatMessage::User(blocks)
            if blocks.iter().any(|b| matches!(b, UserContent::Text(t) if t == needle)))
    })
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

/// Stage 3: `context_tail(since)` returns only feed rows with `seq > since`,
/// still in chronological order. Everything at or before `since` is assumed
/// already folded into a compaction summary the caller holds.
#[sqlx::test]
async fn context_tail_returns_only_rows_after_since(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let store = PgThreadStore::new(pool.clone(), SystemClock::shared());
    let caller = Caller::new(seed.user_id, seed.org_id);
    let agent = seed.agent_id;
    let col_a = resolve_agent_colleague(&pool, seed.org_id, agent)
        .await
        .expect("colleague a");
    let col_h = resolve_user_colleague(&pool, seed.org_id, seed.user_id)
        .await
        .expect("human colleague");
    let thread = store
        .create_thread(&caller, None, None, col_h, Some(col_a))
        .await
        .expect("thread");
    store
        .resolve_participation(&caller, thread, agent)
        .await
        .expect("participation");

    for text in ["m1", "m2", "m3", "m4"] {
        store
            .append(&caller, thread, posted(col_h, Some(col_a), text, false))
            .await
            .expect("append");
    }
    let seqs = thread_seqs(&pool, thread).await;
    assert_eq!(seqs.len(), 4);
    let since = seqs[1]; // after m2

    let tail = store
        .context_tail(
            thread,
            agent,
            col_a,
            since,
            &std::collections::HashMap::new(),
        )
        .await
        .expect("tail");
    assert_eq!(tail.len(), 2, "only m3 + m4 are past `since`");
    let messages = tail.into_messages();
    assert!(!user_text_present(&messages, "m1"));
    assert!(!user_text_present(&messages, "m2"));
    // Returned in chronological order (m3 before m4), not just present.
    let i3 = user_text_index(&messages, "m3").expect("m3 present");
    let i4 = user_text_index(&messages, "m4").expect("m4 present");
    assert!(i3 < i4, "rows must come back in chronological seq order");
}

/// Stage 4: the windowing floor. A thread longer than `MAX_CONTEXT_MESSAGES`,
/// with no summary, still yields a bounded tail — the most-recent window, oldest
/// rows dropped. This is the correctness guarantee that holds with no LLM.
#[sqlx::test]
async fn context_tail_enforces_the_windowing_floor(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let store = PgThreadStore::new(pool.clone(), SystemClock::shared());
    let caller = Caller::new(seed.user_id, seed.org_id);
    let agent = seed.agent_id;
    let col_a = resolve_agent_colleague(&pool, seed.org_id, agent)
        .await
        .expect("colleague a");
    let col_h = resolve_user_colleague(&pool, seed.org_id, seed.user_id)
        .await
        .expect("human colleague");
    let thread = store
        .create_thread(&caller, None, None, col_h, Some(col_a))
        .await
        .expect("thread");
    store
        .resolve_participation(&caller, thread, agent)
        .await
        .expect("participation");

    let cap = usize::try_from(MAX_CONTEXT_MESSAGES).expect("cap fits usize");
    let total = cap + 5;
    for i in 1..=total {
        store
            .append(
                &caller,
                thread,
                posted(col_h, Some(col_a), &format!("m{i}"), false),
            )
            .await
            .expect("append");
    }

    let tail = store
        .context_tail(
            thread,
            agent,
            col_a,
            Seq::ZERO,
            &std::collections::HashMap::new(),
        )
        .await
        .expect("tail");
    assert_eq!(tail.len(), cap, "tail is capped at the windowing floor");
    let messages = tail.into_messages();
    assert!(
        !user_text_present(&messages, "m1"),
        "the oldest message is windowed out"
    );
    assert!(
        user_text_present(&messages, &format!("m{total}")),
        "the most-recent message is kept"
    );
}

/// Stage 6: an oversized `tool_result` is render-capped for the prompt, but the
/// underlying `thread_messages` row is never mutated — a re-read returns the
/// original bytes. Lossless: reduced for context, fully recoverable from the feed.
#[sqlx::test]
async fn heavy_tool_result_is_capped_for_prompt_but_feed_unchanged(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let store = PgThreadStore::new(pool.clone(), SystemClock::shared());
    let caller = Caller::new(seed.user_id, seed.org_id);
    let agent = seed.agent_id;
    let col_a = resolve_agent_colleague(&pool, seed.org_id, agent)
        .await
        .expect("colleague a");
    let col_h = resolve_user_colleague(&pool, seed.org_id, seed.user_id)
        .await
        .expect("human colleague");
    let thread = store
        .create_thread(&caller, None, None, col_h, Some(col_a))
        .await
        .expect("thread");
    store
        .resolve_participation(&caller, thread, agent)
        .await
        .expect("participation");

    let big = "Z".repeat(MAX_TOOL_RESULT_CHARS + 10_000);
    store
        .append(&caller, thread, tool_use(agent, "c1"))
        .await
        .expect("tool_use");
    store
        .append(&caller, thread, tool_result_with(agent, "c1", &big))
        .await
        .expect("tool_result");

    // Prompt rendering: capped + recoverability marker, smaller than the original.
    let tail = store
        .context_tail(
            thread,
            agent,
            col_a,
            Seq::ZERO,
            &std::collections::HashMap::new(),
        )
        .await
        .expect("tail");
    let rendered = first_tool_result_output(&tail.into_messages()).expect("a tool_result");
    assert!(
        rendered.chars().count() < big.chars().count(),
        "rendered body is capped"
    );
    assert!(
        rendered.contains("omitted"),
        "carries the recoverability marker"
    );

    // The immutable feed row is untouched — re-read returns the full bytes.
    let body: serde_json::Value = sqlx::query_scalar(
        "SELECT body FROM thread_messages WHERE thread_id = $1 AND kind = 'tool_result'",
    )
    .bind(thread)
    .fetch_one(&pool)
    .await
    .expect("raw row");
    let stored: ChatMessage = serde_json::from_value(body).expect("decode stored");
    let stored_output = first_tool_result_output(std::slice::from_ref(&stored)).expect("stored");
    assert_eq!(stored_output, big, "feed row is byte-for-byte unchanged");
}

/// Stage 7: the compaction store round-trips. `save_compaction` upserts and
/// advances `covers_through_seq`; `bump_cooldown` records a failure without
/// disturbing the summary; a later `save` clears the cooldown.
#[sqlx::test]
async fn compaction_store_round_trips(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let store = PgThreadStore::new(pool.clone(), SystemClock::shared());
    let caller = Caller::new(seed.user_id, seed.org_id);
    let agent = seed.agent_id;
    let col_a = resolve_agent_colleague(&pool, seed.org_id, agent)
        .await
        .expect("colleague a");
    let col_h = resolve_user_colleague(&pool, seed.org_id, seed.user_id)
        .await
        .expect("human colleague");
    let thread = store
        .create_thread(&caller, None, None, col_h, Some(col_a))
        .await
        .expect("thread");

    // Cold: no compaction yet.
    assert!(
        store
            .load_compaction(thread, agent)
            .await
            .expect("load")
            .is_none(),
        "no compaction before the first save"
    );

    // First save.
    store
        .save_compaction(
            seed.org_id,
            thread,
            agent,
            "summary v1",
            Seq::try_from(5).expect("valid seq"),
            10,
        )
        .await
        .expect("save v1");
    let c = store
        .load_compaction(thread, agent)
        .await
        .expect("load")
        .expect("some");
    assert_eq!(c.summary, "summary v1");
    assert_eq!(c.covers_through_seq, Seq::try_from(5).expect("valid seq"));
    assert_eq!(c.failed_attempts, 0);
    assert!(c.cooldown_until.is_none());

    // Upsert advances covers_through_seq and replaces the summary.
    store
        .save_compaction(
            seed.org_id,
            thread,
            agent,
            "summary v2",
            Seq::try_from(9).expect("valid seq"),
            12,
        )
        .await
        .expect("save v2");
    let c = store
        .load_compaction(thread, agent)
        .await
        .expect("load")
        .expect("some");
    assert_eq!(c.summary, "summary v2");
    assert_eq!(c.covers_through_seq, Seq::try_from(9).expect("valid seq"));

    // A failure bumps the cooldown but leaves the summary/coverage intact.
    let until = chrono::DateTime::from_timestamp(2_000_000_000, 0).expect("ts");
    store
        .bump_cooldown(seed.org_id, thread, agent, until)
        .await
        .expect("bump");
    let c = store
        .load_compaction(thread, agent)
        .await
        .expect("load")
        .expect("some");
    assert_eq!(c.failed_attempts, 1);
    assert_eq!(c.cooldown_until, Some(until));
    assert_eq!(
        c.summary, "summary v2",
        "cooldown doesn't disturb the summary"
    );
    assert_eq!(c.covers_through_seq, Seq::try_from(9).expect("valid seq"));

    // A subsequent success clears the cooldown.
    store
        .save_compaction(
            seed.org_id,
            thread,
            agent,
            "summary v3",
            Seq::try_from(20).expect("valid seq"),
            15,
        )
        .await
        .expect("save v3");
    let c = store
        .load_compaction(thread, agent)
        .await
        .expect("load")
        .expect("some");
    assert_eq!(c.failed_attempts, 0);
    assert!(c.cooldown_until.is_none());
}

/// Stage 7: `bump_cooldown` on a thread with no prior compaction inserts a
/// minimal (empty-summary) cooldown row, so a cold thread whose first
/// compaction fails still backs off instead of retrying every turn.
#[sqlx::test]
async fn bump_cooldown_inserts_when_cold(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let store = PgThreadStore::new(pool.clone(), SystemClock::shared());
    let caller = Caller::new(seed.user_id, seed.org_id);
    let agent = seed.agent_id;
    let col_a = resolve_agent_colleague(&pool, seed.org_id, agent)
        .await
        .expect("colleague a");
    let col_h = resolve_user_colleague(&pool, seed.org_id, seed.user_id)
        .await
        .expect("human colleague");
    let thread = store
        .create_thread(&caller, None, None, col_h, Some(col_a))
        .await
        .expect("thread");

    let until = chrono::DateTime::from_timestamp(2_000_000_000, 0).expect("ts");
    store
        .bump_cooldown(seed.org_id, thread, agent, until)
        .await
        .expect("bump cold");
    let c = store
        .load_compaction(thread, agent)
        .await
        .expect("load")
        .expect("some");
    assert_eq!(c.summary, "");
    assert_eq!(c.covers_through_seq, Seq::ZERO);
    assert_eq!(c.failed_attempts, 1);
    assert_eq!(c.cooldown_until, Some(until));
}
