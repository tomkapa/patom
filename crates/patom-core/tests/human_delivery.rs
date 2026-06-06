//! End-to-end test for `send_message(Human, ...)` and the
//! `ResponseChunk::AgentMessage` it publishes on the root SSE stream
//! (build step §6).
//!
//! Setup: scripted provider that, on the very first turn, emits a
//! `send_message` tool call addressed to the human, and on the second turn
//! ends the turn with text (which the model intends as a private thought —
//! the spec is explicit that delivery happens via `send_message`, not via
//! `Done.final_text`).
//!
//! We then enqueue a prompt, subscribe to the SSE stream on the request id
//! (which is the DAG root for a fresh enqueue), and assert that an
//! `AgentMessage` chunk arrives with the expected `from` and `content`.

#![allow(clippy::expect_used)]

use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use patom::provider::{
    AssistantContent, ChatMessage, ChatResponse, StopReason, ToolCall, ToolCallId, UserContent,
};
use patom::runtime::queue::PromptQueue as _;
use patom::runtime::{IdempotencyKey, NewPromptRequest, ResponseChunk, StreamEvent};
use patom::types::{Prompt, ToolName};
use serde_json::json;
use sqlx::PgPool;

mod common;
use common::harness::{ScriptedProvider, build_harness};

fn send_message_call(content: &str) -> ChatResponse {
    ChatResponse {
        content: vec![AssistantContent::ToolCall(ToolCall {
            id: ToolCallId::try_from("call-1").expect("id"),
            name: ToolName::try_from("send_message").expect("name"),
            input: json!({
                "receiver": { "kind": "human" },
                "content": content,
            }),
        })],
        stop_reason: StopReason::ToolUse,
        ..Default::default()
    }
}

fn final_text(s: &str) -> ChatResponse {
    ChatResponse {
        content: vec![AssistantContent::Text(s.into())],
        stop_reason: StopReason::EndTurn,
        ..Default::default()
    }
}

#[sqlx::test]
async fn send_message_human_publishes_agent_message_on_root_stream(pool: PgPool) {
    let provider = Arc::new(ScriptedProvider::new(vec![
        // Turn 1: model calls send_message(Human, "hello human").
        send_message_call("hello human"),
        // Turn 2: model emits final text. send_message_calls > 0 already so
        // the worker accepts the success path — no ping-pong retry.
        final_text("(internal close-out)"),
    ]));
    let h = build_harness(pool, provider).await;

    let request_id = h
        .queue
        .enqueue(NewPromptRequest {
            session: None,
            sender: h.default_human_participant(),
            receiver_agent_id: h.default_agent_id,
            parent_session: None,
            content: Prompt::try_from("ping the agent").expect("prompt"),
            idempotency_key: IdempotencyKey::try_from("k-human").expect("key"),
            org_id: h.default_org_id,
            created_by_user_id: h.default_user_id,
            kind_payload: patom::runtime::RequestKindPayload::Normal {},
        })
        .await
        .expect("enqueue")
        .request_id();

    // Subscribe to the SSE stream for this request id (== root). Drain
    // chunks until a terminal one (Done/Error) arrives or we time out.
    let chunks = drain_chunks(&h, request_id, Duration::from_secs(8)).await;

    let agent_msg = chunks
        .iter()
        .find_map(|c| match c {
            ResponseChunk::AgentMessage { from, content, .. } => Some((from, content.clone())),
            _ => None,
        })
        .expect("expected an AgentMessage on the root stream");
    let (from, content) = agent_msg;
    assert_eq!(*from, h.default_agent_id, "AgentMessage records caller id");
    assert_eq!(
        content, "hello human",
        "delivered content matches tool input"
    );

    h.workers.shutdown().await;
}

/// Regression: when an agent calls `send_message(Human, …)` from the
/// caller's own human↔agent session (the common chat case),
/// `resolve_or_create_for_pair` returns the same session id. Persisting the
/// outbound message into that same session — sender = agent (the viewer) —
/// makes it render as `Assistant.text` in the agent's next snapshot, which
/// would land *between* the assistant's `tool_call(send_message)` and the
/// matching `tool_result`. OpenAI rejects that wire shape with
/// `An assistant message with 'tool_calls' must be followed by tool messages
/// responding to each 'tool_call_id'`. This test pins that the persisted
/// session keeps the tool_call and tool_result adjacent, so a follow-up
/// turn replays a valid history.
#[sqlx::test]
async fn send_message_human_keeps_tool_call_and_result_adjacent(pool: PgPool) {
    let provider = Arc::new(ScriptedProvider::new(vec![
        send_message_call("hello human"),
        final_text("(internal close-out)"),
    ]));
    let h = build_harness(pool, provider).await;

    let outcome = h
        .queue
        .enqueue(NewPromptRequest {
            session: None,
            sender: h.default_human_participant(),
            receiver_agent_id: h.default_agent_id,
            parent_session: None,
            content: Prompt::try_from("ping the agent").expect("prompt"),
            idempotency_key: IdempotencyKey::try_from("k-adjacent").expect("key"),
            org_id: h.default_org_id,
            created_by_user_id: h.default_user_id,
            kind_payload: patom::runtime::RequestKindPayload::Normal {},
        })
        .await
        .expect("enqueue");
    let request_id = outcome.request_id();
    let session_id = outcome.session();

    // Drain the stream so the worker has finished both turns before we
    // snapshot the session — otherwise we may inspect a half-written log.
    let _ = drain_chunks(&h, request_id, Duration::from_secs(8)).await;

    let viewer = h.default_agent_participant();
    let messages = h
        .sessions
        .snapshot(session_id, viewer.colleague_id().expect("real colleague"))
        .await
        .expect("snapshot");

    let tool_call_idx = messages
        .iter()
        .position(|m| {
            matches!(
                m,
                ChatMessage::Assistant(blocks)
                    if blocks.iter().any(|b| matches!(b, AssistantContent::ToolCall(_)))
            )
        })
        .expect("expected an Assistant message containing the send_message tool_call");
    let next = messages
        .get(tool_call_idx + 1)
        .expect("expected a message after the tool_call");
    assert!(
        matches!(
            next,
            ChatMessage::User(blocks)
                if blocks.iter().any(|b| matches!(b, UserContent::ToolResult(_)))
        ),
        "tool_call must be immediately followed by its tool_result; got {next:?} \
         (full snapshot: {messages:?})",
    );

    h.workers.shutdown().await;
}

/// Regression: the second human prompt in an existing session must be able
/// to publish its `AgentMessage` chunk. Pre-fix `send_message` published
/// to the session's stored `root_request_id` (= the *first* prompt's id),
/// whose sink had been closed at quiescence — the second turn's publish
/// failed with "request … stream already closed", the tool result came
/// back as `is_error: true`, and the agent's outbound message was
/// silently dropped from the live stream. Post-fix the chunk lands on the
/// current claim's request id (an open sink), so the user's UI sees it.
#[sqlx::test]
async fn followup_prompt_publishes_agent_message_on_open_sink(pool: PgPool) {
    let provider = Arc::new(ScriptedProvider::new(vec![
        // Prompt #1
        send_message_call("hello human"),
        final_text("(internal close-out 1)"),
        // Prompt #2 — fresh script entries because the cursor advances
        send_message_call("hello again"),
        final_text("(internal close-out 2)"),
    ]));
    let h = build_harness(pool, provider).await;

    let first = h
        .queue
        .enqueue(NewPromptRequest {
            session: None,
            sender: h.default_human_participant(),
            receiver_agent_id: h.default_agent_id,
            parent_session: None,
            content: Prompt::try_from("first prompt").expect("prompt"),
            idempotency_key: IdempotencyKey::try_from("k-first").expect("key"),
            org_id: h.default_org_id,
            created_by_user_id: h.default_user_id,
            kind_payload: patom::runtime::RequestKindPayload::Normal {},
        })
        .await
        .expect("enqueue first");
    let session_id = first.session();
    let _ = drain_chunks(&h, first.request_id(), Duration::from_secs(8)).await;

    let second = h
        .queue
        .enqueue(NewPromptRequest {
            session: Some(session_id),
            sender: h.default_human_participant(),
            receiver_agent_id: h.default_agent_id,
            parent_session: None,
            content: Prompt::try_from("second prompt").expect("prompt"),
            idempotency_key: IdempotencyKey::try_from("k-second").expect("key"),
            org_id: h.default_org_id,
            created_by_user_id: h.default_user_id,
            kind_payload: patom::runtime::RequestKindPayload::Normal {},
        })
        .await
        .expect("enqueue second");

    let chunks = drain_chunks(&h, second.request_id(), Duration::from_secs(8)).await;

    let agent_msg = chunks.iter().find_map(|c| match c {
        ResponseChunk::AgentMessage { from, content, .. } => Some((from, content.clone())),
        _ => None,
    });
    let (from, content) = agent_msg.unwrap_or_else(|| {
        panic!("second prompt's stream must carry the AgentMessage; chunks={chunks:?}")
    });
    assert_eq!(*from, h.default_agent_id);
    assert_eq!(content, "hello again");

    h.workers.shutdown().await;
}

async fn drain_chunks(
    h: &common::harness::WorkerHarness,
    id: patom::runtime::PromptRequestId,
    deadline: Duration,
) -> Vec<ResponseChunk> {
    let source: patom::runtime::SharedResponseSource = h.hub.clone();
    let mut stream = source.subscribe(id, None).await.expect("subscribe");
    let mut got = Vec::new();
    let until = std::time::Instant::now() + deadline;
    while std::time::Instant::now() < until {
        let next = tokio::time::timeout(Duration::from_millis(200), stream.next()).await;
        let Ok(Some(item)) = next else { continue };
        let ev = item.expect("ok");
        if let StreamEvent::Chunk(env) = ev {
            let terminal = env.chunk.is_terminal();
            got.push(env.chunk);
            if terminal {
                return got;
            }
        }
    }
    got
}
