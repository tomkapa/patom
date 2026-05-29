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
use sqlx::PgPool;

mod common;
use common::pg::seed_tenant;

fn queue(pool: &PgPool) -> Arc<PgPromptQueue> {
    Arc::new(PgPromptQueue::new(pool.clone(), SystemClock::shared()))
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

#[sqlx::test]
async fn pending_row_keeps_dag_live(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let q = queue(&pool);
    let dag = PgDagBudget::new(pool.clone());
    let root = enqueue_root(&q, seed.agent_id, seed.org_id, seed.user_id).await;

    assert!(
        !dag.quiescent(root).await.expect("query"),
        "fresh enqueue leaves a pending row"
    );
}

#[sqlx::test]
async fn processing_row_keeps_dag_live(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let q = queue(&pool);
    let dag = PgDagBudget::new(pool.clone());
    let root = enqueue_root(&q, seed.agent_id, seed.org_id, seed.user_id).await;
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

#[sqlx::test]
async fn done_row_drains_dag(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let q = queue(&pool);
    let dag = PgDagBudget::new(pool.clone());
    let root = enqueue_root(&q, seed.agent_id, seed.org_id, seed.user_id).await;
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

#[sqlx::test]
async fn unknown_root_is_quiescent(pool: PgPool) {
    // EXISTS over an empty match set is FALSE → quiescent = TRUE. Useful
    // for the worker's quiescence trigger when a synthetic test root has no
    // `prompt_requests` rows at all.
    let _seed = seed_tenant(&pool).await;
    let dag = PgDagBudget::new(pool.clone());
    let phantom = PromptRequestId::new();
    assert!(dag.quiescent(phantom).await.expect("query"));
}

#[sqlx::test]
async fn second_pending_row_still_blocks_quiescence(pool: PgPool) {
    // Multi-prompt on the same DAG: while one row is done, another is still
    // pending → DAG is non-quiescent. Confirms the EXISTS scope is the
    // entire DAG, not a single receipt.
    let seed = seed_tenant(&pool).await;
    let q = queue(&pool);
    let dag = PgDagBudget::new(pool.clone());
    let root = enqueue_root(&q, seed.agent_id, seed.org_id, seed.user_id).await;

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
        receiver_agent_id: seed.agent_id,
        parent_session: None,
        content: Prompt::try_from("again").expect("prompt"),
        idempotency_key: IdempotencyKey::try_from("second").expect("key"),
        org_id: seed.org_id,
        created_by_user_id: seed.user_id,
        kind_payload: patom_rs::runtime::RequestKindPayload::Normal {},
    })
    .await
    .expect("second enqueue");

    assert!(
        !dag.quiescent(root).await.expect("query"),
        "second pending row keeps the DAG live",
    );
}
