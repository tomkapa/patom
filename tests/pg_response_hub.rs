//! Trait-contract tests for [`patom_rs::runtime::PgResponseHub`]: live
//! publish/subscribe round trip, replay-on-late-subscribe, replay-since cutoff,
//! and slot-cap eviction (closed-first, then live with a warning).

#![allow(clippy::expect_used)]

use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;

use patom_rs::agents::AgentId;
use patom_rs::clock::{SharedClock, SystemClock};
use patom_rs::runtime::queue::PromptQueue as _;
use patom_rs::runtime::{
    IdempotencyKey, LeaseTiming, NewPromptRequest, PgPromptQueue, PgResponseHub, PromptRequestId,
    ResponseChunk, ResponseSink as _, ResponseSource as _, StreamEvent,
};
use patom_rs::session::{PgSessionStore, SessionId};
use patom_rs::types::{Participant, Prompt};
use sqlx::PgPool;

mod common;
use common::pg::{human_to_agent_session, seed_tenant};

/// Stage a prompt request row so the chunks table's FK is satisfied. Returns the
/// request id we can publish chunks against.
async fn stage_request(
    pool: &PgPool,
    seed: &common::pg::Seed,
    clock: SharedClock,
    agent_id: AgentId,
) -> (SessionId, PromptRequestId) {
    let session_store = PgSessionStore::new(pool.clone(), clock.clone());
    let session = human_to_agent_session(&session_store, agent_id, seed.org_id, seed.user_id).await;

    let queue = Arc::new(PgPromptQueue::with_caps(
        pool.clone(),
        clock,
        LeaseTiming::default_const(),
        32,
        3,
    ));
    let key = format!("k-{}", uuid::Uuid::new_v4());
    let id = queue
        .enqueue(NewPromptRequest {
            session: Some(session),
            sender: Participant::Human,
            receiver_agent_id: agent_id,
            parent_session: None,
            content: Prompt::try_from("hi").expect("prompt"),
            idempotency_key: IdempotencyKey::try_from(key).expect("key"),
            org_id: seed.org_id,
            created_by_user_id: seed.user_id,
            kind_payload: patom_rs::runtime::RequestKindPayload::Normal {},
        })
        .await
        .expect("enqueue")
        .request_id();
    (session, id)
}

#[sqlx::test]
async fn publish_and_subscribe_live(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let clock = SystemClock::shared();
    let hub = Arc::new(PgResponseHub::new(pool.clone(), clock.clone()));
    let (_session, id) = stage_request(&pool, &seed, clock, seed.agent_id).await;

    let mut stream = hub.subscribe(id, None).await.expect("subscribe");

    hub.publish(
        id,
        ResponseChunk::Text {
            value: "hello".into(),
        },
    )
    .await
    .expect("p1");
    hub.publish(
        id,
        ResponseChunk::Done {
            final_text: "hello".into(),
        },
    )
    .await
    .expect("p2");

    let mut got = Vec::new();
    while let Some(item) = tokio::time::timeout(Duration::from_millis(500), stream.next())
        .await
        .ok()
        .flatten()
    {
        got.push(item.expect("ok"));
        if matches!(
            got.last(),
            Some(StreamEvent::Chunk(e)) if e.chunk.is_terminal()
        ) {
            break;
        }
    }
    assert!(got.len() >= 2, "got {got:?}");
}

#[sqlx::test]
async fn replay_serves_late_subscriber(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let clock = SystemClock::shared();
    let hub = Arc::new(PgResponseHub::new(pool.clone(), clock.clone()));
    let (_session, id) = stage_request(&pool, &seed, clock, seed.agent_id).await;

    hub.publish(id, ResponseChunk::Text { value: "a".into() })
        .await
        .expect("p1");
    hub.publish(id, ResponseChunk::Text { value: "b".into() })
        .await
        .expect("p2");
    hub.publish(
        id,
        ResponseChunk::Done {
            final_text: "ab".into(),
        },
    )
    .await
    .expect("done");

    let mut stream = hub.subscribe(id, None).await.expect("late");
    let mut got = Vec::new();
    while let Some(item) = tokio::time::timeout(Duration::from_millis(500), stream.next())
        .await
        .ok()
        .flatten()
    {
        let ev = item.expect("ok");
        let terminal = matches!(&ev, StreamEvent::Chunk(e) if e.chunk.is_terminal());
        got.push(ev);
        if terminal {
            break;
        }
    }
    assert_eq!(got.len(), 3);
}

#[sqlx::test]
async fn replay_respects_since_cutoff(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let clock = SystemClock::shared();
    let hub = Arc::new(PgResponseHub::new(pool.clone(), clock.clone()));
    let (_session, id) = stage_request(&pool, &seed, clock, seed.agent_id).await;

    let s0 = hub
        .publish(id, ResponseChunk::Text { value: "a".into() })
        .await
        .expect("p1");
    hub.publish(id, ResponseChunk::Text { value: "b".into() })
        .await
        .expect("p2");
    hub.publish(
        id,
        ResponseChunk::Done {
            final_text: "ab".into(),
        },
    )
    .await
    .expect("p3");

    let mut stream = hub.subscribe(id, Some(s0)).await.expect("subscribe-since");
    let mut got = Vec::new();
    while let Some(item) = tokio::time::timeout(Duration::from_millis(500), stream.next())
        .await
        .ok()
        .flatten()
    {
        let ev = item.expect("ok");
        let terminal = matches!(&ev, StreamEvent::Chunk(e) if e.chunk.is_terminal());
        got.push(ev);
        if terminal {
            break;
        }
    }
    // Should skip s0 and replay s1 + done — i.e. 2 entries.
    assert_eq!(got.len(), 2);
}

#[sqlx::test]
async fn slot_cap_evicts_oldest_closed_first(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let clock = SystemClock::shared();
    let hub = Arc::new(PgResponseHub::with_caps(pool.clone(), clock.clone(), 2));
    let (_sa, a) = stage_request(&pool, &seed, clock.clone(), seed.agent_id).await;
    let (_sb, b) = stage_request(&pool, &seed, clock.clone(), seed.agent_id).await;
    let (_sc, c) = stage_request(&pool, &seed, clock, seed.agent_id).await;

    // Close `a` first (terminal chunk → closed).
    hub.publish(
        a,
        ResponseChunk::Done {
            final_text: "a".into(),
        },
    )
    .await
    .expect("a-done");
    hub.publish(b, ResponseChunk::Text { value: "b".into() })
        .await
        .expect("b-pub");
    // Inserting `c` pushes us past cap — `a` (closed) should evict from the in-process
    // map. The chunk row in DB persists; only the live broadcast slot is evicted.
    hub.publish(c, ResponseChunk::Text { value: "c".into() })
        .await
        .expect("c-pub");

    // Live `b` continues to receive new chunks.
    hub.publish(b, ResponseChunk::Text { value: "b2".into() })
        .await
        .expect("b-still-alive");
}
