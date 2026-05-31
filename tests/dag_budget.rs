//! Trait-contract tests for [`DagBudget`] / [`PgDagBudget`].
//!
//! Covers the spec's atomic budget semantics: `bump_or_fail` succeeds while
//! `turns_used < turns_cap`, returns `DagBudgetExceeded` at the cap, and
//! distinguishes a missing row (`DagNotFound`) from an exhausted one. The
//! quiescence query is exercised by the dedicated `tests/quiescence.rs`.

#![allow(clippy::expect_used)]

use std::sync::Arc;

use patom_rs::clock::SystemClock;
use patom_rs::runtime::queue::PromptQueue as _;
use patom_rs::runtime::{
    DagBudget, IdempotencyKey, NewPromptRequest, PgDagBudget, PgPromptQueue, PromptError,
    PromptRequestId,
};
use patom_rs::types::{Participant, Prompt};
use sqlx::PgPool;

mod common;
use common::pg::seed_tenant;

/// Mint a fresh DAG anchor by enqueuing a brand-new request (`session: None`)
/// — the queue creates the session and seeds `prompt_request_dags` in one
/// transaction. Returns the root_request_id (== the new request's id).
async fn seed_dag(pool: &PgPool, seed: &common::pg::Seed) -> PromptRequestId {
    // Unique idempotency key per call so two seed_dag invocations in the
    // same test don't collapse onto one row.
    let queue = Arc::new(PgPromptQueue::new(pool.clone(), SystemClock::shared()));
    let outcome = queue
        .enqueue(NewPromptRequest {
            session: None,
            sender: Participant::Human,
            receiver_agent_id: seed.agent_id,
            parent_session: None,
            content: Prompt::try_from("hi").expect("prompt"),
            idempotency_key: IdempotencyKey::try_from(format!("dag-seed-{}", uuid::Uuid::new_v4()))
                .expect("key"),
            org_id: seed.org_id,
            created_by_user_id: seed.user_id,
            kind_payload: patom_rs::runtime::RequestKindPayload::Normal {},
        })
        .await
        .expect("enqueue");
    outcome.request_id()
}

/// Force a small budget cap on the seeded DAG row. The default
/// `MAX_DAG_TURNS = 64` is too many to bump in a unit test; we shrink the
/// cap directly so cap behaviour is exercised in two bumps.
async fn shrink_cap(pool: &PgPool, root: PromptRequestId, cap: i64) {
    sqlx::query("UPDATE prompt_request_dags SET turns_cap = $1 WHERE root_request_id = $2")
        .bind(cap)
        .bind(root)
        .execute(pool)
        .await
        .expect("shrink cap");
}

#[sqlx::test]
async fn bump_succeeds_while_under_cap(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let dag = PgDagBudget::new(pool.clone());
    let root = seed_dag(&pool, &seed).await;
    shrink_cap(&pool, root, 3).await;

    let first = dag.bump_or_fail(root).await.expect("first");
    assert_eq!(first.turns_used, 1);
    assert_eq!(first.turns_cap, 3);
    let second = dag.bump_or_fail(root).await.expect("second");
    assert_eq!(second.turns_used, 2);
    let third = dag.bump_or_fail(root).await.expect("third");
    assert_eq!(third.turns_used, 3);
}

#[sqlx::test]
async fn bump_at_cap_returns_dag_budget_exceeded(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let dag = PgDagBudget::new(pool.clone());
    let root = seed_dag(&pool, &seed).await;
    shrink_cap(&pool, root, 1).await;

    dag.bump_or_fail(root).await.expect("first under cap");
    let err = dag.bump_or_fail(root).await.expect_err("at cap");
    match err {
        PromptError::DagBudgetExceeded {
            turns_used,
            turns_cap,
            ..
        } => {
            assert_eq!(turns_used, 1);
            assert_eq!(turns_cap, 1);
        }
        other => panic!("expected DagBudgetExceeded, got {other:?}"),
    }
}

#[sqlx::test]
async fn bump_unknown_root_returns_dag_not_found(pool: PgPool) {
    let _seed = seed_tenant(&pool).await;
    let dag = PgDagBudget::new(pool.clone());
    // No seed — pure synthetic id; no row in prompt_request_dags.
    let phantom = PromptRequestId::new();
    let err = dag.bump_or_fail(phantom).await.expect_err("phantom");
    assert!(
        matches!(err, PromptError::DagNotFound(id) if id == phantom),
        "want DagNotFound, got {err:?}",
    );
}

#[sqlx::test]
async fn bumps_are_independent_across_dags(pool: PgPool) {
    // Two seeded DAGs with their own caps. A bump on one cannot affect the
    // other's turns_used — the row's PK is `root_request_id`.
    let seed = seed_tenant(&pool).await;
    let dag = PgDagBudget::new(pool.clone());
    let root_a = seed_dag(&pool, &seed).await;
    let root_b = seed_dag(&pool, &seed).await;
    shrink_cap(&pool, root_a, 5).await;
    shrink_cap(&pool, root_b, 5).await;

    dag.bump_or_fail(root_a).await.expect("a1");
    dag.bump_or_fail(root_a).await.expect("a2");
    let b1 = dag.bump_or_fail(root_b).await.expect("b1");
    assert_eq!(b1.turns_used, 1, "b's counter starts at 0 not 2");
}
