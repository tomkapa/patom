//! P3: a root trigger (human @tag / scheduled fire) mints its own
//! `prompt_request_dags` budget. One human message tagging two agents mints two
//! independent DAGs, each capped at `MAX_DAG_TURNS`.

mod common;

use patom::agents::AgentId;
use patom::auth::Caller;
use patom::clock::SystemClock;
use patom::colleagues::resolve_user_colleague;
use patom::runtime::{IdempotencyKey, NewTrigger, PgPromptQueue, PromptQueue, RequestKindPayload};
use patom::threads::{AgentThreadId, PgThreadStore, ThreadStore};
use sqlx::PgPool;
use uuid::Uuid;

use common::pg::seed_tenant;

#[sqlx::test]
async fn two_tags_one_message_mint_two_dags(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let store = PgThreadStore::new(pool.clone(), SystemClock::shared());
    let queue = PgPromptQueue::new(pool.clone(), SystemClock::shared());
    let caller = Caller::new(seed.user_id, seed.org_id);
    let human = resolve_user_colleague(&pool, seed.org_id, seed.user_id)
        .await
        .expect("human colleague");

    let agent_a = seed.agent_id;
    let agent_b = AgentId::new();
    sqlx::query(
        "INSERT INTO agents (id, name, is_default, created_at, updated_at, description, org_id) \
         VALUES ($1, 'agent-b', false, now(), now(), 'Agent B', $2)",
    )
    .bind(agent_b)
    .bind(seed.org_id)
    .execute(&pool)
    .await
    .expect("insert agent b");

    let thread = store
        .create_thread(&caller, None, None, human)
        .await
        .expect("thread");
    let state_a = store
        .resolve_participation(&caller, thread, agent_a)
        .await
        .expect("a");
    let state_b = store
        .resolve_participation(&caller, thread, agent_b)
        .await
        .expect("b");

    let trig = |agent: AgentId, state: AgentThreadId| NewTrigger {
        org_id: seed.org_id,
        acting_user_id: seed.user_id,
        thread_id: Some(thread),
        state_id: Some(state),
        background_turn_id: None,
        sender_colleague_id: human,
        receiver_agent_id: agent,
        root_request_id: None, // root mint
        trigger_message_id: None,
        idempotency_key: IdempotencyKey::try_from(format!("tag-{}", Uuid::new_v4())).expect("key"),
        kind_payload: RequestKindPayload::Normal {},
    };

    let r_a = queue
        .enqueue_trigger(trig(agent_a, state_a))
        .await
        .expect("enqueue a");
    let r_b = queue
        .enqueue_trigger(trig(agent_b, state_b))
        .await
        .expect("enqueue b");

    let dags: Vec<(Uuid, i64)> =
        sqlx::query_as("SELECT root_request_id, turns_cap FROM prompt_request_dags")
            .fetch_all(&pool)
            .await
            .expect("read dags");

    assert_eq!(
        dags.len(),
        2,
        "@A @B in one message mints two independent DAGs"
    );
    assert!(
        dags.iter().all(|(_, cap)| *cap == 64),
        "each DAG capped at MAX_DAG_TURNS (64)"
    );
    let roots: std::collections::HashSet<Uuid> = dags.iter().map(|(r, _)| *r).collect();
    assert!(
        roots.contains(&r_a.as_uuid()),
        "DAG anchored on A's trigger id"
    );
    assert!(
        roots.contains(&r_b.as_uuid()),
        "DAG anchored on B's trigger id"
    );
}
