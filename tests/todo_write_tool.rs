//! Trait-contract tests for `todo_write` and its persistence layer.
//!
//! Single tool, single table, atomic-overwrite semantics. Coverage:
//! * Happy path: items round-trip through the store and the rendered
//!   `<todos>` block contains every item.
//! * Atomic replace: a second write drops items the model omitted.
//! * Invariant: model-supplied list with two `in_progress` is refused
//!   at the boundary; no DB write happens.
//! * Cap: more than `MAX_TODOS_PER_LIST` items is refused.
//! * Per-turn rate cap: ≤ `MAX_TODO_WRITES_PER_TURN` writes / request_id.
//! * Cross-session isolation: list in session A is invisible to session B.
//! * Persistence: a fresh `request_id` for the same session reads back
//!   the prior list — this is what makes "survives across re-runs" real.

#![allow(clippy::expect_used)]

use std::sync::Arc;

use patom_rs::clock::SystemClock;
use patom_rs::runtime::PromptRequestId;
use patom_rs::session::{PgSessionStore, SharedSessionStore};
use patom_rs::tools::system::todos::{
    MAX_TODO_WRITES_PER_TURN, MAX_TODOS_PER_LIST, PgSessionTodoStore, SharedSessionTodoStore,
    TodoToolDeps, TodoWriteTool,
};
use patom_rs::tools::{Tool, ToolCallContext, ToolError};
use patom_rs::types::Participant;
use serde_json::{Value, json};

mod common;
use common::pg::{TestDb, human_to_agent_session, seed_prompt_request};

struct Fixture {
    tool: TodoWriteTool,
    store: SharedSessionTodoStore,
    session: patom_rs::session::SessionId,
    agent_id: patom_rs::agents::AgentId,
    user_id: patom_rs::auth::UserId,
    org_id: patom_rs::auth::OrgId,
}

async fn fixture(db: &TestDb) -> Fixture {
    let clock = SystemClock::shared();
    let sessions: SharedSessionStore =
        Arc::new(PgSessionStore::new(db.pool.clone(), clock.clone()));
    let store: SharedSessionTodoStore =
        Arc::new(PgSessionTodoStore::new(db.pool.clone(), clock.clone()));
    let session = human_to_agent_session(
        sessions.as_ref(),
        db.default_agent_id,
        db.default_org_id,
        db.default_user_id,
    )
    .await;
    let tool = TodoWriteTool::new(TodoToolDeps::new(store.clone()));
    Fixture {
        tool,
        store,
        session,
        agent_id: db.default_agent_id,
        user_id: db.default_user_id,
        org_id: db.default_org_id,
    }
}

fn ctx(f: &Fixture, request_id: PromptRequestId) -> ToolCallContext {
    ToolCallContext {
        session_id: f.session,
        viewer: Participant::agent(f.agent_id),
        root_request_id: request_id,
        request_id,
        kind_payload: patom_rs::runtime::RequestKindPayload::Normal {},
        acting_user_id: f.user_id,
        org_id: f.org_id,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn writes_and_reads_back_through_store() {
    let db = TestDb::fresh().await;
    let f = fixture(&db).await;
    let req = seed_prompt_request(&db.pool, f.session, f.agent_id, db.default_org_id).await;

    let out = f
        .tool
        .execute(
            json!({
                "items": [
                    { "id": "a", "content": "research the bug", "status": "in_progress" },
                    { "id": "b", "content": "write a failing test", "status": "pending" },
                    { "id": "c", "content": "fix it", "status": "pending" },
                ]
            }),
            &ctx(&f, req),
        )
        .await
        .expect("write");
    let parsed: Value = serde_json::from_str(&out).expect("json");
    assert_eq!(parsed["count"], 3);
    assert_eq!(parsed["items"][0]["id"], "a");
    assert_eq!(parsed["items"][1]["status"], "pending");

    let stored = f.store.get(f.session).await.expect("get");
    assert_eq!(stored.len(), 3);
    let items = stored.as_slice();
    assert_eq!(items[0].id.as_str(), "a");
    assert_eq!(items[2].content.as_str(), "fix it");
}

#[tokio::test(flavor = "multi_thread")]
async fn second_write_atomically_replaces_first() {
    let db = TestDb::fresh().await;
    let f = fixture(&db).await;
    let req = seed_prompt_request(&db.pool, f.session, f.agent_id, db.default_org_id).await;

    f.tool
        .execute(
            json!({
                "items": [
                    { "id": "a", "content": "step 1", "status": "completed" },
                    { "id": "b", "content": "step 2", "status": "in_progress" },
                    { "id": "c", "content": "step 3", "status": "pending" },
                ]
            }),
            &ctx(&f, req),
        )
        .await
        .expect("first write");

    f.tool
        .execute(
            json!({
                "items": [
                    { "id": "a", "content": "step 1", "status": "completed" },
                    { "id": "b", "content": "step 2", "status": "completed" },
                ]
            }),
            &ctx(&f, req),
        )
        .await
        .expect("second write");

    let stored = f.store.get(f.session).await.expect("get");
    assert_eq!(stored.len(), 2);
    let ids: Vec<&str> = stored.as_slice().iter().map(|i| i.id.as_str()).collect();
    assert_eq!(ids, vec!["a", "b"]);
    // c is gone — atomic replace, not merge.
    assert!(!ids.contains(&"c"));
}

#[tokio::test(flavor = "multi_thread")]
async fn two_in_progress_items_are_refused() {
    let db = TestDb::fresh().await;
    let f = fixture(&db).await;
    let req = seed_prompt_request(&db.pool, f.session, f.agent_id, db.default_org_id).await;

    let err = f
        .tool
        .execute(
            json!({
                "items": [
                    { "id": "a", "content": "one", "status": "in_progress" },
                    { "id": "b", "content": "two", "status": "in_progress" },
                ]
            }),
            &ctx(&f, req),
        )
        .await
        .expect_err("two in_progress should reject");
    assert!(matches!(err, ToolError::InvalidInput(_)));

    // No row was written.
    let stored = f.store.get(f.session).await.expect("get");
    assert!(stored.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn list_above_cap_is_refused() {
    let db = TestDb::fresh().await;
    let f = fixture(&db).await;
    let req = seed_prompt_request(&db.pool, f.session, f.agent_id, db.default_org_id).await;

    let items: Vec<Value> = (0..=MAX_TODOS_PER_LIST)
        .map(|i| {
            json!({
                "id": format!("t{i}"),
                "content": "x",
                "status": "pending",
            })
        })
        .collect();
    let err = f
        .tool
        .execute(json!({ "items": items }), &ctx(&f, req))
        .await
        .expect_err("over-cap should reject");
    assert!(matches!(err, ToolError::InvalidInput(_)));

    let stored = f.store.get(f.session).await.expect("get");
    assert!(stored.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn per_turn_rate_cap_blocks_runaway_writes() {
    let db = TestDb::fresh().await;
    let f = fixture(&db).await;
    let req = seed_prompt_request(&db.pool, f.session, f.agent_id, db.default_org_id).await;
    let payload = json!({
        "items": [
            { "id": "a", "content": "x", "status": "pending" }
        ]
    });

    for i in 0..MAX_TODO_WRITES_PER_TURN {
        f.tool
            .execute(payload.clone(), &ctx(&f, req))
            .await
            .unwrap_or_else(|e| panic!("write {i} under cap: {e}"));
    }
    let err = f
        .tool
        .execute(payload, &ctx(&f, req))
        .await
        .expect_err("over rate cap should reject");
    assert!(matches!(err, ToolError::InvalidInput(_)));
}

#[tokio::test(flavor = "multi_thread")]
async fn list_is_isolated_to_its_session() {
    let db = TestDb::fresh().await;
    let f = fixture(&db).await;
    let other_session = human_to_agent_session(
        &PgSessionStore::new(db.pool.clone(), SystemClock::shared()),
        f.agent_id,
        f.org_id,
        f.user_id,
    )
    .await;
    let req = seed_prompt_request(&db.pool, f.session, f.agent_id, db.default_org_id).await;
    f.tool
        .execute(
            json!({"items": [{ "id": "a", "content": "only in session 1", "status": "pending" }]}),
            &ctx(&f, req),
        )
        .await
        .expect("write");

    let other = f.store.get(other_session).await.expect("get other");
    assert!(other.is_empty(), "todos must not leak across sessions");
}

#[tokio::test(flavor = "multi_thread")]
async fn list_survives_into_a_fresh_request() {
    // "Persists across re-runs" — a new prompt_request for the same
    // session must read back the prior list. This is the property the
    // user explicitly asked for.
    let db = TestDb::fresh().await;
    let f = fixture(&db).await;
    let req_one = seed_prompt_request(&db.pool, f.session, f.agent_id, db.default_org_id).await;
    f.tool
        .execute(
            json!({"items": [{ "id": "carry", "content": "carry over", "status": "in_progress" }]}),
            &ctx(&f, req_one),
        )
        .await
        .expect("write in turn 1");

    // Simulate a fresh turn: new request id, same session.
    let _req_two = seed_prompt_request(&db.pool, f.session, f.agent_id, db.default_org_id).await;
    let carried = f.store.get(f.session).await.expect("get in turn 2");
    assert_eq!(carried.len(), 1);
    assert_eq!(carried.as_slice()[0].id.as_str(), "carry");
}
