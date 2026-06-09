//! P10 rehome opening test: the live stream is keyed on `thread_id`.
//!
//! Before the rehome the SSE fan-in (`PgThreadStream`) demuxed by
//! `root_request_id` and `pg_response::publish` named the dropped `session_id`
//! column (runtime-broken since migration 63). After it: `publish` reads the
//! request's `thread_id`, the NOTIFY carries it, and a subscriber on that thread
//! receives every chunk published on any request in the thread.
//!
//! This is the smallest expression of that contract: enqueue a chat trigger
//! carrying thread T, subscribe to T, publish a chunk on the trigger's request,
//! and assert the subscriber sees it tagged with T.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::time::Duration;

use futures::StreamExt;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;

use patom::auth::Caller;
use patom::clock::SystemClock;
use patom::colleagues::resolve_user_colleague;
use patom::runtime::{
    IdempotencyKey, LeaseTiming, NewTrigger, PgPromptQueue, PgResponseHub, PgThreadStream,
    PromptQueue, RequestKindPayload, ResponseChunk, ResponseSink as _, ThreadStreamEvent,
};
use patom::threads::{PgThreadStore, ThreadStore};
use sqlx::PgPool;

mod common;
use common::pg::seed_tenant;

#[sqlx::test]
async fn publish_on_a_request_reaches_a_subscriber_on_its_thread(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let clock = SystemClock::shared();
    let caller = Caller::new(seed.user_id, seed.org_id);

    // A DM thread + the agent's participation in it.
    let threads = PgThreadStore::new(pool.clone(), clock.clone());
    let human_col = resolve_user_colleague(&pool, seed.org_id, seed.user_id)
        .await
        .expect("human colleague");
    let thread = threads
        .create_thread(&caller, None, None, human_col)
        .await
        .expect("create thread");
    let state_id = threads
        .resolve_participation(&caller, thread, seed.agent_id)
        .await
        .expect("resolve participation");

    // A chat trigger anchored on this thread → a `prompt_requests` row whose
    // `thread_id` is T. This is what `publish` reads to route the NOTIFY.
    let queue = PgPromptQueue::with_caps(
        pool.clone(),
        clock.clone(),
        LeaseTiming::default_const(),
        32,
        3,
    );
    let request_id = queue
        .enqueue_trigger(NewTrigger {
            org_id: seed.org_id,
            acting_user_id: seed.user_id,
            thread_id: Some(thread),
            state_id: Some(state_id),
            background_turn_id: None,
            sender_colleague_id: human_col,
            receiver_agent_id: seed.agent_id,
            root_request_id: None,
            trigger_message_id: None,
            idempotency_key: IdempotencyKey::try_from(format!("k-{}", uuid::Uuid::new_v4()))
                .expect("idempotency key"),
            kind_payload: RequestKindPayload::Normal {},
        })
        .await
        .expect("enqueue trigger");

    // Spawn the fan-in listener and subscribe to the thread BEFORE publishing so
    // the per-thread slot has a live receiver when the NOTIFY lands.
    let cancel = CancellationToken::new();
    let stream = PgThreadStream::spawn(pool.clone(), cancel.clone())
        .await
        .expect("spawn thread stream");
    let mut sub = stream.subscribe(thread);

    // Publish a chunk on the trigger's request; the NOTIFY carries the request's
    // thread_id.
    let hub = PgResponseHub::new(pool.clone(), clock.clone());
    hub.publish(
        request_id,
        ResponseChunk::Text {
            value: "hello thread".to_owned(),
        },
    )
    .await
    .expect("publish");

    // The subscriber on T receives the chunk, tagged with T and the request.
    let event = timeout(Duration::from_secs(5), sub.next())
        .await
        .expect("stream item before timeout")
        .expect("stream not closed")
        .expect("stream item ok");
    match event {
        ThreadStreamEvent::Item(item) => {
            assert_eq!(item.thread_id, thread, "item routed to its thread");
            assert_eq!(item.request_id, request_id, "item carries its request id");
            match item.chunk {
                ResponseChunk::Text { value } => assert_eq!(value, "hello thread"),
                other => panic!("unexpected chunk: {other:?}"),
            }
            assert_eq!(
                item.from_agent, seed.agent_id,
                "authored by the turn's agent"
            );
        }
        ThreadStreamEvent::Stalled => panic!("unexpected stall"),
    }

    cancel.cancel();
}
