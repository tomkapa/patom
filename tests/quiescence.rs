//! Trait-contract tests for [`DagBudget::quiescent`].
//!
//! Covers the EXISTS query that gates the worker's terminal-`Done`
//! emission: a DAG with any `pending` or `processing` row in
//! `prompt_requests` is non-quiescent; once every row reaches
//! `done`/`failed` the DAG is considered drained.

#![allow(clippy::expect_used)]

use std::sync::Arc;

use patom_rs::clock::SystemClock;
use patom_rs::runtime::queue::PromptQueue as _;
use patom_rs::runtime::{
    DagBudget, IdempotencyKey, NewPromptRequest, PgDagBudget, PgPromptQueue, PromptRequestId,
    WorkerId,
};
use patom_rs::types::{Participant, Prompt};

mod common;
use common::pg::TestDb;

fn queue(db: &TestDb) -> Arc<PgPromptQueue> {
    Arc::new(PgPromptQueue::new(db.pool.clone(), SystemClock::shared()))
}

async fn enqueue_root(
    q: &Arc<PgPromptQueue>,
    agent_id: patom_rs::agents::AgentId,
    org_id: patom_rs::auth::OrgId,
    user_id: patom_rs::auth::UserId,
) -> PromptRequestId {
    q.enqueue(NewPromptRequest {
        session: None,
        sender: Participant::Human,
        receiver_agent_id: agent_id,
        parent_session: None,
        content: Prompt::try_from("hi").expect("prompt"),
        idempotency_key: IdempotencyKey::try_from(format!("k-{}", uuid::Uuid::new_v4()))
            .expect("key"),
        org_id,
        created_by_user_id: user_id,
        kind_payload: patom_rs::runtime::RequestKindPayload::Normal {},
    })
    .await
    .expect("enqueue")
    .request_id()
}

#[tokio::test(flavor = "multi_thread")]
async fn pending_row_keeps_dag_live() {
    let db = TestDb::fresh().await;
    let q = queue(&db);
    let dag = PgDagBudget::new(db.pool.clone());
    let root = enqueue_root(
        &q,
        db.default_agent_id,
        db.default_org_id,
        db.default_user_id,
    )
    .await;

    assert!(
        !dag.quiescent(root).await.expect("query"),
        "fresh enqueue leaves a pending row"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn processing_row_keeps_dag_live() {
    let db = TestDb::fresh().await;
    let q = queue(&db);
    let dag = PgDagBudget::new(db.pool.clone());
    let root = enqueue_root(
        &q,
        db.default_agent_id,
        db.default_org_id,
        db.default_user_id,
    )
    .await;
    // Claim moves the row from pending → processing without finishing it.
    let _ = q
        .claim_next_session(WorkerId::new())
        .await
        .expect("claim")
        .expect("some");
    assert!(
        !dag.quiescent(root).await.expect("query"),
        "claimed row is processing — still live"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn done_row_drains_dag() {
    let db = TestDb::fresh().await;
    let q = queue(&db);
    let dag = PgDagBudget::new(db.pool.clone());
    let root = enqueue_root(
        &q,
        db.default_agent_id,
        db.default_org_id,
        db.default_user_id,
    )
    .await;
    let claim = q
        .claim_next_session(WorkerId::new())
        .await
        .expect("claim")
        .expect("some");
    q.mark_done(&claim.receipt()).await.expect("mark_done");
    assert!(
        dag.quiescent(root).await.expect("query"),
        "every row terminal => quiescent",
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn unknown_root_is_quiescent() {
    // EXISTS over an empty match set is FALSE → quiescent = TRUE. Useful
    // for the worker's quiescence trigger when a synthetic test root has no
    // `prompt_requests` rows at all.
    let db = TestDb::fresh().await;
    let dag = PgDagBudget::new(db.pool.clone());
    let phantom = PromptRequestId::new();
    assert!(dag.quiescent(phantom).await.expect("query"));
}

#[tokio::test(flavor = "multi_thread")]
async fn second_pending_row_still_blocks_quiescence() {
    // Multi-prompt on the same DAG: while one row is done, another is still
    // pending → DAG is non-quiescent. Confirms the EXISTS scope is the
    // entire DAG, not a single receipt.
    let db = TestDb::fresh().await;
    let q = queue(&db);
    let dag = PgDagBudget::new(db.pool.clone());
    let root = enqueue_root(
        &q,
        db.default_agent_id,
        db.default_org_id,
        db.default_user_id,
    )
    .await;

    // Claim & mark done the first row.
    let claim = q
        .claim_next_session(WorkerId::new())
        .await
        .expect("claim")
        .expect("some");
    q.mark_done(&claim.receipt()).await.expect("mark_done");

    // Add a second prompt to the same DAG (continuing the same session).
    let session = claim.session;
    q.enqueue(NewPromptRequest {
        session: Some(session),
        sender: Participant::Human,
        receiver_agent_id: db.default_agent_id,
        parent_session: None,
        content: Prompt::try_from("again").expect("prompt"),
        idempotency_key: IdempotencyKey::try_from("second").expect("key"),
        org_id: db.default_org_id,
        created_by_user_id: db.default_user_id,
        kind_payload: patom_rs::runtime::RequestKindPayload::Normal {},
    })
    .await
    .expect("second enqueue");

    assert!(
        !dag.quiescent(root).await.expect("query"),
        "second pending row keeps the DAG live",
    );
}
