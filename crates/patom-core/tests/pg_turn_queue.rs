//! P2 contract tests for the thread-feed claim path: claim-and-drain re-keyed
//! to the polymorphic `claim_key` (= `agent_thread_state.id` for chat turns).
//! Proves coalesce (all pending for one agent → one turn) and serialize (one
//! in-flight turn per `(thread, agent)`, concurrent across agents in a thread).

mod common;

use patom::agents::AgentId;
use patom::auth::{Caller, OrgId, UserId};
use patom::clock::SystemClock;
use patom::colleagues::{ColleagueId, resolve_user_colleague};
use patom::runtime::{
    IdempotencyKey, NewTrigger, PgPromptQueue, PromptRequestId, RequestKindPayload, WorkerId,
};
use patom::threads::{AgentThreadId, PgThreadStore, ThreadId, ThreadStore};
use sqlx::PgPool;
use uuid::Uuid;

use common::pg::seed_tenant;

#[allow(clippy::too_many_arguments)]
async fn enqueue(
    queue: &PgPromptQueue,
    org: OrgId,
    user: UserId,
    sender: ColleagueId,
    thread: ThreadId,
    state: AgentThreadId,
    agent: AgentId,
) -> PromptRequestId {
    let key = IdempotencyKey::try_from(format!("trig-{}", Uuid::new_v4())).expect("idempotency key");
    queue
        .enqueue_trigger(NewTrigger {
            org_id: org,
            acting_user_id: user,
            thread_id: Some(thread),
            state_id: Some(state),
            background_turn_id: None,
            sender_colleague_id: sender,
            receiver_agent_id: agent,
            root_request_id: None,
            trigger_message_id: None,
            idempotency_key: key,
            kind_payload: RequestKindPayload::Normal {},
        })
        .await
        .expect("enqueue trigger")
}

#[sqlx::test]
async fn claim_coalesces_pending_for_one_agent(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let store = PgThreadStore::new(pool.clone(), SystemClock::shared());
    let queue = PgPromptQueue::new(pool.clone(), SystemClock::shared());
    let caller = Caller::new(seed.user_id, seed.org_id);
    let human = resolve_user_colleague(&pool, seed.org_id, seed.user_id).await.expect("human colleague");

    let thread = store.create_thread(&caller, None, None, human).await.expect("thread");
    let state_a = store.resolve_participation(&caller, thread, seed.agent_id).await.expect("participation");

    // Two pending triggers for the same (thread, agent).
    enqueue(&queue, seed.org_id, seed.user_id, human, thread, state_a, seed.agent_id).await;
    enqueue(&queue, seed.org_id, seed.user_id, human, thread, state_a, seed.agent_id).await;

    let claimed = queue.claim_next_turn(WorkerId::new()).await.expect("claim").expect("a turn");
    assert_eq!(claimed.claim_key, state_a.as_uuid(), "keyed by (thread, agent)");
    assert_eq!(claimed.trigger_ids.len(), 2, "both pending triggers coalesce into one turn");
    assert_eq!(claimed.receiver_agent_id, seed.agent_id);
    assert_eq!(claimed.thread_id, Some(thread));
}

#[sqlx::test]
async fn claim_serializes_per_thread_agent(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let store = PgThreadStore::new(pool.clone(), SystemClock::shared());
    let queue = PgPromptQueue::new(pool.clone(), SystemClock::shared());
    let caller = Caller::new(seed.user_id, seed.org_id);
    let human = resolve_user_colleague(&pool, seed.org_id, seed.user_id).await.expect("human colleague");

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

    let thread = store.create_thread(&caller, None, None, human).await.expect("thread");
    let state_a = store.resolve_participation(&caller, thread, agent_a).await.expect("a");
    let state_b = store.resolve_participation(&caller, thread, agent_b).await.expect("b");

    enqueue(&queue, seed.org_id, seed.user_id, human, thread, state_a, agent_a).await;
    enqueue(&queue, seed.org_id, seed.user_id, human, thread, state_b, agent_b).await;

    // Two agents in ONE thread both claim concurrently (not serialized per thread).
    let c1 = queue.claim_next_turn(WorkerId::new()).await.expect("c1").expect("first turn");
    let c2 = queue.claim_next_turn(WorkerId::new()).await.expect("c2").expect("second turn");
    assert_ne!(c1.claim_key, c2.claim_key, "distinct (thread, agent) turns run concurrently");
    let mut got = [c1.claim_key, c2.claim_key];
    got.sort();
    let mut want = [state_a.as_uuid(), state_b.as_uuid()];
    want.sort();
    assert_eq!(got, want, "both participations claimed");

    // A second trigger for the already-leased agent A is NOT re-claimed.
    enqueue(&queue, seed.org_id, seed.user_id, human, thread, state_a, agent_a).await;
    let c3 = queue.claim_next_turn(WorkerId::new()).await.expect("c3");
    assert!(c3.is_none(), "a trigger for a leased (thread, agent) waits — serialized per agent");
}
