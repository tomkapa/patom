//! End-to-end test for the worker's ping-pong retry guard (build step §7).
//!
//! Boots a real Postgres pipeline with a [`ScriptedProvider`] that returns
//! plain assistant text on every call — never a `send_message` tool call.
//! The worker should:
//! 1. observe `AgentReply.send_message_calls == 0` and inject a system
//!    nudge into the receiver's session,
//! 2. retry up to `MAX_PINGPONG_RETRIES` times,
//! 3. park the request as `Failed` with `FailureReason::NoEgress` after the
//!    cap.
//!
//! We then assert the request lands in `Failed` and the provider was called
//! exactly `MAX_PINGPONG_RETRIES + 1` times (initial reply + retries).

#![allow(clippy::expect_used)]

use std::sync::Arc;
use std::time::Duration;

use patom_rs::provider::{AssistantContent, ChatResponse, StopReason};
use patom_rs::runtime::queue::PromptQueue as _;
use patom_rs::runtime::{
    FailureReason, IdempotencyKey, MAX_PINGPONG_RETRIES, NewPromptRequest, RequestStatus,
    RequestStatusView,
};
use patom_rs::types::{Participant, Prompt};
use sqlx::PgPool;

mod common;
use common::harness::{ScriptedProvider, build_harness};

fn text(s: &str) -> ChatResponse {
    ChatResponse {
        content: vec![AssistantContent::Text(s.into())],
        stop_reason: StopReason::EndTurn,
        ..Default::default()
    }
}

#[sqlx::test]
async fn agent_text_without_send_message_parks_as_no_egress(pool: PgPool) {
    // The agent returns text every turn — the worker should treat each one
    // as a non-delivery and ultimately fail with `NoEgress`. The initial
    // reply plus `MAX_PINGPONG_RETRIES` retries gives `cap + 1` total
    // provider calls.
    let cap = usize::from(MAX_PINGPONG_RETRIES);
    let mut script = Vec::with_capacity(cap + 1);
    for i in 0..=cap {
        script.push(text(&format!("private thought #{i}")));
    }
    let provider = Arc::new(ScriptedProvider::new(script));
    let h = build_harness(pool, provider.clone()).await;

    let request_id = h
        .queue
        .enqueue(NewPromptRequest {
            session: None,
            sender: Participant::Human,
            receiver_agent_id: h.default_agent_id,
            parent_session: None,
            content: Prompt::try_from("hi").expect("prompt"),
            idempotency_key: IdempotencyKey::try_from("k1").expect("key"),
            org_id: h.default_org_id,
            created_by_user_id: h.default_user_id,
            kind_payload: patom_rs::runtime::RequestKindPayload::Normal {},
        })
        .await
        .expect("enqueue")
        .request_id();

    let view = await_terminal(&h, request_id, Duration::from_secs(8)).await;
    assert!(
        matches!(view.status, RequestStatus::Failed),
        "expected Failed; got {:?}",
        view.status,
    );
    let reason = view.failure_reason.expect("reason recorded");
    assert!(
        matches!(reason, FailureReason::NoEgress),
        "expected NoEgress; got {reason:?}",
    );
    assert_eq!(
        provider.calls(),
        cap + 1,
        "worker should drive the model once + retry MAX_PINGPONG_RETRIES times",
    );

    h.workers.shutdown().await;
}

async fn await_terminal(
    h: &common::harness::WorkerHarness,
    id: patom_rs::runtime::PromptRequestId,
    deadline: Duration,
) -> RequestStatusView {
    let until = std::time::Instant::now() + deadline;
    while std::time::Instant::now() < until {
        let view = h.queue.status(id).await.expect("status");
        if matches!(view.status, RequestStatus::Done | RequestStatus::Failed) {
            return view;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    h.queue.status(id).await.expect("status")
}
