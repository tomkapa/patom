//! Trait-contract tests for [`patom::runtime::PgPromptQueue`]: idempotent
//! enqueue, claim-and-drain, lease fencing, orphan recovery, attempts cap → poison,
//! pending cap. Each test uses a fresh schema; lease-expiry tests use a `TestClock`
//! so they don't burn wall-clock seconds.

#![allow(clippy::expect_used)]

use std::sync::Arc;
use std::time::Duration;

use patom::agents::AgentId;
use patom::auth::{OrgId, UserId};
use patom::clock::{SharedClock, TestClock};
use patom::runtime::queue::PromptQueue as _;
use patom::runtime::{
    IdempotencyKey, LeaseManager as _, LeaseTiming, NewPromptRequest, PgPromptQueue, PromptError,
    PromptRequestId, RequestStatus, WorkerId,
};
use patom::session::{PgSessionStore, SessionId};
use patom::types::Prompt;
use sqlx::PgPool;

mod common;
use common::pg::{human_to_agent_session, seed_tenant};

const LEASE_TTL: Duration = Duration::from_secs(2);
const HEARTBEAT: Duration = Duration::from_millis(500);

struct Fixture {
    pool: PgPool,
    seed: common::pg::Seed,
    queue: Arc<PgPromptQueue>,
    clock: Arc<TestClock>,
    session: SessionId,
    agent_id: AgentId,
}

async fn fresh(pool: PgPool) -> Fixture {
    let seed = seed_tenant(&pool).await;
    let test_clock = Arc::new(TestClock::new());
    let clock: SharedClock = test_clock.clone();
    let session_store = PgSessionStore::new(pool.clone(), clock.clone());
    let session =
        human_to_agent_session(&pool, &session_store, seed.agent_id, seed.org_id, seed.user_id).await;
    let timing = LeaseTiming::try_new(LEASE_TTL, HEARTBEAT).expect("valid timing");
    let queue = Arc::new(PgPromptQueue::with_caps(pool.clone(), clock, timing, 32, 3));
    let agent_id = seed.agent_id;
    Fixture {
        pool,
        seed,
        queue,
        clock: test_clock,
        session,
        agent_id,
    }
}

fn req(
    session: SessionId,
    sender: patom::types::Participant,
    agent_id: AgentId,
    content: &str,
    key: &str,
    org_id: OrgId,
    user_id: UserId,
) -> NewPromptRequest {
    NewPromptRequest {
        session: Some(session),
        sender,
        receiver_agent_id: agent_id,
        parent_session: None,
        content: Prompt::try_from(content).expect("p"),
        idempotency_key: IdempotencyKey::try_from(key).expect("k"),
        org_id,
        created_by_user_id: user_id,
        kind_payload: patom::runtime::RequestKindPayload::Normal {},
    }
}

#[sqlx::test]
async fn enqueue_is_idempotent_on_repeat_key(pool: PgPool) {
    let f = fresh(pool).await;
    let (q, s, agent_id) = (&f.queue, f.session, f.agent_id);
    let first = q
        .enqueue(req(s, common::pg::human_participant(&f.pool, f.seed.org_id, f.seed.user_id).await, agent_id, "hi", "k1", f.seed.org_id, f.seed.user_id))
        .await
        .expect("first");
    let second = q
        .enqueue(req(
            s,
            common::pg::human_participant(&f.pool, f.seed.org_id, f.seed.user_id).await,
            agent_id,
            "hi-again",
            "k1",
            f.seed.org_id,
            f.seed.user_id,
        ))
        .await
        .expect("second");
    assert_eq!(first.request_id(), second.request_id());
}

#[sqlx::test]
async fn claim_drains_all_pending_for_session(pool: PgPool) {
    let f = fresh(pool).await;
    let (q, s, agent_id) = (&f.queue, f.session, f.agent_id);
    let r1 = q
        .enqueue(req(s, common::pg::human_participant(&f.pool, f.seed.org_id, f.seed.user_id).await, agent_id, "a", "k1", f.seed.org_id, f.seed.user_id))
        .await
        .expect("ok")
        .request_id();
    let r2 = q
        .enqueue(req(s, common::pg::human_participant(&f.pool, f.seed.org_id, f.seed.user_id).await, agent_id, "b", "k2", f.seed.org_id, f.seed.user_id))
        .await
        .expect("ok")
        .request_id();
    let claimed = q
        .claim_next_session(WorkerId::new())
        .await
        .expect("claim")
        .expect("some");
    assert_eq!(claimed.prompts.len(), 2);
    let ids: Vec<PromptRequestId> = claimed.prompts.iter().map(|p| p.request_id).collect();
    assert!(ids.contains(&r1));
    assert!(ids.contains(&r2));
}

#[sqlx::test]
async fn second_claim_skips_leased_session(pool: PgPool) {
    let f = fresh(pool).await;
    let (q, s, agent_id) = (&f.queue, f.session, f.agent_id);
    let _ = q
        .enqueue(req(s, common::pg::human_participant(&f.pool, f.seed.org_id, f.seed.user_id).await, agent_id, "a", "k1", f.seed.org_id, f.seed.user_id))
        .await
        .expect("ok");
    let _first = q
        .claim_next_session(WorkerId::new())
        .await
        .expect("claim")
        .expect("some");
    let second = q.claim_next_session(WorkerId::new()).await.expect("claim2");
    assert!(second.is_none());
}

#[sqlx::test]
async fn lease_expiry_returns_orphan_to_pending(pool: PgPool) {
    let f = fresh(pool).await;
    let (q, clock, s, agent_id) = (&f.queue, &f.clock, f.session, f.agent_id);
    let _ = q
        .enqueue(req(s, common::pg::human_participant(&f.pool, f.seed.org_id, f.seed.user_id).await, agent_id, "a", "k1", f.seed.org_id, f.seed.user_id))
        .await
        .expect("ok");
    let _ = q
        .claim_next_session(WorkerId::new())
        .await
        .expect("claim")
        .expect("some");

    clock.advance(LEASE_TTL + Duration::from_secs(1));

    let again = q
        .claim_next_session(WorkerId::new())
        .await
        .expect("reclaim")
        .expect("orphan recovered");
    assert_eq!(again.prompts.len(), 1);
}

#[sqlx::test]
async fn mark_done_with_stale_token_fails(pool: PgPool) {
    let f = fresh(pool).await;
    let (q, clock, s, agent_id) = (&f.queue, &f.clock, f.session, f.agent_id);
    let _ = q
        .enqueue(req(s, common::pg::human_participant(&f.pool, f.seed.org_id, f.seed.user_id).await, agent_id, "a", "k1", f.seed.org_id, f.seed.user_id))
        .await
        .expect("ok")
        .request_id();
    let claim1 = q
        .claim_next_session(WorkerId::new())
        .await
        .expect("c1")
        .expect("some");

    clock.advance(LEASE_TTL + Duration::from_secs(1));

    let _claim2 = q
        .claim_next_session(WorkerId::new())
        .await
        .expect("c2")
        .expect("some");

    let receipt = claim1.receipt();
    let err = q.mark_done(&receipt).await.expect_err("stale");
    assert!(matches!(err, PromptError::LeaseStale { .. }));
}

#[sqlx::test]
async fn poisons_after_max_attempts_via_orphan_path(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let test_clock = Arc::new(TestClock::new());
    let clock: SharedClock = test_clock.clone();
    let session_store = PgSessionStore::new(pool.clone(), clock.clone());
    let agent_id = seed.agent_id;
    let session = human_to_agent_session(&pool, &session_store, agent_id, seed.org_id, seed.user_id).await;
    let timing = LeaseTiming::try_new(LEASE_TTL, HEARTBEAT).expect("timing");
    let q = Arc::new(PgPromptQueue::with_caps(pool.clone(), clock, timing, 8, 2));
    let r = q
        .enqueue(req(session, common::pg::human_participant(&pool, seed.org_id, seed.user_id).await, agent_id, "a", "k1", seed.org_id, seed.user_id))
        .await
        .expect("ok")
        .request_id();

    // Attempt 1: claim, then let lease expire.
    let _ = q.claim_next_session(WorkerId::new()).await.expect("c1");
    test_clock.advance(LEASE_TTL + Duration::from_secs(1));

    // Attempt 2: claim again, let lease expire — now hits the cap.
    let _ = q
        .claim_next_session(WorkerId::new())
        .await
        .expect("c2")
        .expect("some");
    test_clock.advance(LEASE_TTL + Duration::from_secs(1));

    // A third claim resets orphans; the poisoned row is now `failed`.
    let _ = q.claim_next_session(WorkerId::new()).await.expect("c3");
    let view = q.status(r).await.expect("status");
    assert!(matches!(view.status, RequestStatus::Failed));
    assert!(view.failure_reason.is_some());
}

#[sqlx::test]
async fn heartbeat_extends_lease(pool: PgPool) {
    let f = fresh(pool).await;
    let (q, clock, s, agent_id) = (&f.queue, &f.clock, f.session, f.agent_id);
    let _ = q
        .enqueue(req(s, common::pg::human_participant(&f.pool, f.seed.org_id, f.seed.user_id).await, agent_id, "a", "k1", f.seed.org_id, f.seed.user_id))
        .await
        .expect("ok");
    let claim = q
        .claim_next_session(WorkerId::new())
        .await
        .expect("c")
        .expect("some");

    clock.advance(LEASE_TTL.saturating_sub(Duration::from_millis(200)));
    q.heartbeat(&claim.lease).await.expect("heartbeat");
    clock.advance(LEASE_TTL.saturating_sub(Duration::from_millis(200)));
    q.heartbeat(&claim.lease).await.expect("heartbeat2");

    q.mark_done(&claim.receipt()).await.expect("done");
}

#[sqlx::test]
async fn release_clears_lease_so_others_can_claim(pool: PgPool) {
    let f = fresh(pool).await;
    let (q, s, agent_id) = (&f.queue, f.session, f.agent_id);
    let _ = q
        .enqueue(req(s, common::pg::human_participant(&f.pool, f.seed.org_id, f.seed.user_id).await, agent_id, "a", "k1", f.seed.org_id, f.seed.user_id))
        .await
        .expect("ok");
    let claim = q
        .claim_next_session(WorkerId::new())
        .await
        .expect("c")
        .expect("some");
    q.mark_done(&claim.receipt()).await.expect("done");
    q.release(&claim.lease).await.expect("release");

    let session_store = PgSessionStore::new(f.pool.clone(), f.clock.clone());
    let s2 = human_to_agent_session(&f.pool, 
        &session_store,
        f.seed.agent_id,
        f.seed.org_id,
        f.seed.user_id,
    )
    .await;
    let _ = q
        .enqueue(req(s2, common::pg::human_participant(&f.pool, f.seed.org_id, f.seed.user_id).await, agent_id, "b", "k2", f.seed.org_id, f.seed.user_id))
        .await
        .expect("ok");
    let again = q.claim_next_session(WorkerId::new()).await.expect("c2");
    assert!(again.is_some());
}

#[sqlx::test]
async fn enqueue_caps_pending_per_session(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let test_clock = Arc::new(TestClock::new());
    let clock: SharedClock = test_clock;
    let session_store = PgSessionStore::new(pool.clone(), clock.clone());
    let agent_id = seed.agent_id;
    let session = human_to_agent_session(&pool, &session_store, agent_id, seed.org_id, seed.user_id).await;
    let timing = LeaseTiming::try_new(LEASE_TTL, HEARTBEAT).expect("valid timing");
    let q = Arc::new(PgPromptQueue::with_caps(pool.clone(), clock, timing, 2, 3));
    q.enqueue(req(session, common::pg::human_participant(&pool, seed.org_id, seed.user_id).await, agent_id, "a", "k1", seed.org_id, seed.user_id))
        .await
        .expect("ok1");
    q.enqueue(req(session, common::pg::human_participant(&pool, seed.org_id, seed.user_id).await, agent_id, "b", "k2", seed.org_id, seed.user_id))
        .await
        .expect("ok2");
    let err = q
        .enqueue(req(session, common::pg::human_participant(&pool, seed.org_id, seed.user_id).await, agent_id, "c", "k3", seed.org_id, seed.user_id))
        .await
        .expect_err("over cap");
    assert!(matches!(err, PromptError::PendingCapExceeded { .. }));
}
