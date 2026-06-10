//! P10/G2: the canonical flat thread feed.
//!
//! `ThreadStore::feed` returns one ordered page of a thread's `thread_messages`
//! — posted chat from every party *plus* agent private artifacts (reasoning /
//! tool_use / …) with their `kind` exposed for the FE — in `seq` order, with
//! each row's sender resolved to its colleague identity. This is the data path
//! behind `GET /threads/{id}/messages`; the multi-party ordering + identity is
//! what the legacy `session_messages`-per-pair read could not express.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::sync::Arc;

use patom::auth::{Caller, OrgId, UserId};
use patom::channels::ChannelId;
use patom::clock::SystemClock;
use patom::colleagues::{ColleagueId, resolve_agent_colleague, resolve_user_colleague};
use patom::provider::{AssistantContent, ChatMessage, UserContent};
use patom::threads::{MessageKind, NewMessage, PgThreadStore, SharedThreadStore};
use patom::types::MessageSender;
use sqlx::PgPool;

mod common;
use common::pg::seed_tenant;

/// Add a second org member (auto-enrolled into `#general` by the membership
/// trigger) and return its colleague id — a distinct human party in the feed.
async fn seed_second_human(pool: &PgPool, org_id: OrgId) -> ColleagueId {
    let user_id = UserId::new();
    sqlx::query(
        "INSERT INTO users (id, email, display_name, created_at, updated_at) \
         VALUES ($1, $2, 'Second', now(), now())",
    )
    .bind(user_id)
    .bind(format!(
        "second-{}@example.test",
        user_id.as_uuid().simple()
    ))
    .execute(pool)
    .await
    .expect("insert second user");
    sqlx::query(
        "INSERT INTO org_members (org_id, user_id, role, created_at) VALUES ($1, $2, 'member', now())",
    )
    .bind(org_id)
    .bind(user_id)
    .execute(pool)
    .await
    .expect("insert second membership");
    resolve_user_colleague(pool, org_id, user_id)
        .await
        .expect("second human colleague")
}

fn posted(sender: ColleagueId, text: &str) -> NewMessage {
    NewMessage {
        kind: MessageKind::Posted,
        sender: Some(sender),
        owner_agent_id: None,
        receiver: None,
        body: ChatMessage::User(vec![UserContent::Text(text.into())]),
        request_id: None,
        idempotency_key: None,
    }
}

#[sqlx::test]
async fn g2_canonical_feed_seq_order_multi_party(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let clock = SystemClock::shared();
    let threads: SharedThreadStore = Arc::new(PgThreadStore::new(pool.clone(), clock.clone()));
    let caller = Caller::new(seed.user_id, seed.org_id);

    let human_a = resolve_user_colleague(&pool, seed.org_id, seed.user_id)
        .await
        .expect("human A colleague");
    let agent_col = resolve_agent_colleague(&pool, seed.org_id, seed.agent_id)
        .await
        .expect("agent colleague");
    let human_b = seed_second_human(&pool, seed.org_id).await;

    // A channel thread in `#general` (A + B are both members via the trigger).
    let (general,): (uuid::Uuid,) =
        sqlx::query_as("SELECT id FROM channels WHERE org_id = $1 AND name = 'general'")
            .bind(seed.org_id)
            .fetch_one(&pool)
            .await
            .expect("general channel");
    let general = ChannelId::from(general);
    let thread = threads
        .create_thread(&caller, Some(general), None, human_a, None)
        .await
        .expect("thread");

    // A multi-party feed: human A posts, the agent thinks (owner-private
    // reasoning) then posts, human B posts. Four rows, three distinct authors.
    threads
        .append(&caller, thread, posted(human_a, "hi from A"))
        .await
        .expect("a post");
    threads
        .append(
            &caller,
            thread,
            NewMessage {
                kind: MessageKind::Reasoning,
                sender: None,
                owner_agent_id: Some(seed.agent_id),
                receiver: None,
                body: ChatMessage::Assistant(vec![AssistantContent::Reasoning(
                    "let me check".into(),
                )]),
                request_id: None,
                idempotency_key: None,
            },
        )
        .await
        .expect("agent reasoning");
    threads
        .append(&caller, thread, posted(agent_col, "agent reply"))
        .await
        .expect("agent post");
    threads
        .append(&caller, thread, posted(human_b, "hi from B"))
        .await
        .expect("b post");

    let feed = threads
        .feed(&caller, thread, None, 100)
        .await
        .expect("feed");

    // Exactly the four rows, oldest→newest, seqs strictly increasing.
    assert_eq!(
        feed.len(),
        4,
        "feed returns every row, not a per-pair slice"
    );
    let seqs: Vec<i64> = feed.iter().map(|m| m.seq).collect();
    let mut sorted = seqs.clone();
    sorted.sort_unstable();
    assert_eq!(seqs, sorted, "feed is in ascending seq order");

    // Row 0: human A's post.
    let r0 = &feed[0];
    assert_eq!(r0.kind, MessageKind::Posted);
    assert!(matches!(r0.sender, MessageSender::Human { .. }));
    assert_eq!(r0.sender.colleague_id(), Some(human_a));
    assert_eq!(r0.sender.user_id(), Some(seed.user_id));

    // Row 1: the agent's owner-private reasoning — shown in the feed (kind
    // exposed), authored by no colleague (System sender), owned by the agent.
    let r1 = &feed[1];
    assert_eq!(r1.kind, MessageKind::Reasoning);
    assert!(
        matches!(r1.sender, MessageSender::System),
        "reasoning rows have the System sender (no colleague)"
    );
    assert_eq!(r1.owner_agent_id, Some(seed.agent_id));

    // Row 2: the agent's posted reply — sender resolves to the agent.
    let r2 = &feed[2];
    assert_eq!(r2.kind, MessageKind::Posted);
    assert!(matches!(r2.sender, MessageSender::Agent { .. }));
    assert_eq!(r2.sender.agent_id(), Some(seed.agent_id));

    // Row 3: the *second* human — proving the feed is multi-party, not the
    // viewer stamped onto every row (the legacy single-human bug).
    let r3 = &feed[3];
    assert_eq!(r3.kind, MessageKind::Posted);
    assert!(matches!(r3.sender, MessageSender::Human { .. }));
    assert_eq!(r3.sender.colleague_id(), Some(human_b));
    assert_ne!(
        r3.sender.colleague_id(),
        Some(human_a),
        "B is a distinct party from A"
    );
}
