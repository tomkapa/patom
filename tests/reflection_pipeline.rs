//! Trait-contract tests for the reflection pipeline:
//!
//! - The queue's claim path returns `RequestKind::Reflection` rows with
//!   their `kind_payload` and serialises memory-mutating jobs per agent.
//! - `ReflectionScheduler::tick` finds idle sessions whose latest
//!   message is past any existing checkpoint and enqueues exactly one
//!   reflection row.
//! - Repeated ticks against the same idle session do not duplicate.

#![allow(clippy::expect_used)]

use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use patom::clock::{SharedClock, SystemClock};
use patom::memory::ReflectionScheduler;
use patom::runtime::{
    IdempotencyKey, NewPromptRequest, PgPromptQueue, PromptRequestId, RequestKind,
    RequestKindPayload, SharedPromptQueue, WorkerId,
};
use patom::session::{PgSessionStore, SharedSessionStore};
use patom::types::{Participant, Prompt};
use sqlx::PgPool;

mod common;
use common::pg::{human_to_agent_session, seed_tenant};

async fn enqueue_reflection_row(
    queue: &SharedPromptQueue,
    session: patom::session::SessionId,
    agent_id: patom::agents::AgentId,
    up_to: PromptRequestId,
    key: &str,
    org_id: patom::auth::OrgId,
    user_id: patom::auth::UserId,
) -> PromptRequestId {
    let req = NewPromptRequest {
        session: Some(session),
        sender: Participant::agent(agent_id),
        receiver_agent_id: agent_id,
        parent_session: None,
        content: Prompt::try_from("(reflection)").expect("p"),
        idempotency_key: IdempotencyKey::try_from(key).expect("k"),
        org_id,
        created_by_user_id: user_id,
        kind_payload: RequestKindPayload::Reflection {
            session_id: session,
            up_to_turn_id: up_to,
        },
    };
    queue.enqueue(req).await.expect("enqueue").request_id()
}

#[sqlx::test]
async fn claim_returns_reflection_kind_and_payload(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let clock: SharedClock = SystemClock::shared();
    let sessions: SharedSessionStore = Arc::new(PgSessionStore::new(pool.clone(), clock.clone()));
    let queue: SharedPromptQueue = Arc::new(PgPromptQueue::new(pool.clone(), clock));

    let session =
        human_to_agent_session(sessions.as_ref(), seed.agent_id, seed.org_id, seed.user_id).await;
    // The reflection's `since_turn_id` references a real prompt row to
    // satisfy the JSON payload's UUID — any id works since the worker
    // does not dereference it during a claim.
    let since = PromptRequestId::new();
    let _ = enqueue_reflection_row(
        &queue,
        session,
        seed.agent_id,
        since,
        "k1",
        seed.org_id,
        seed.user_id,
    )
    .await;

    let claimed = queue
        .claim_next_session(WorkerId::new())
        .await
        .expect("claim")
        .expect("some");
    assert_eq!(claimed.kind_payload.kind(), RequestKind::Reflection);
    match claimed.kind_payload {
        RequestKindPayload::Reflection {
            session_id,
            up_to_turn_id,
        } => {
            assert_eq!(session_id, session);
            assert_eq!(up_to_turn_id, since);
        }
        other => panic!("unexpected payload {other:?}"),
    }
}

#[sqlx::test]
async fn per_agent_serialization_skips_session_with_in_flight_reflection(pool: PgPool) {
    // A reflection row in `processing` for agent X must lock out any
    // other claim against agent X — even on a different session of the
    // same agent. This protects the journal from concurrent reflection
    // mutations.
    let seed = seed_tenant(&pool).await;
    let clock: SharedClock = SystemClock::shared();
    let sessions: SharedSessionStore = Arc::new(PgSessionStore::new(pool.clone(), clock.clone()));
    let queue: SharedPromptQueue = Arc::new(PgPromptQueue::new(pool.clone(), clock));

    let agent_id = seed.agent_id;
    let s1 = human_to_agent_session(sessions.as_ref(), agent_id, seed.org_id, seed.user_id).await;

    // Enqueue + claim the first reflection — it goes to `processing`
    // and holds a lease.
    let _ = enqueue_reflection_row(
        &queue,
        s1,
        agent_id,
        PromptRequestId::new(),
        "k1",
        seed.org_id,
        seed.user_id,
    )
    .await;
    let first = queue
        .claim_next_session(WorkerId::new())
        .await
        .expect("claim")
        .expect("some");
    assert_eq!(first.kind_payload.kind(), RequestKind::Reflection);

    // A normal prompt on a different session for the SAME agent should
    // claim fine — only memory-mutating kinds are serialised per agent.
    let s2_root = PromptRequestId::new();
    let _ = sessions
        .resolve_or_create_for_pair(
            s2_root,
            Participant::Human,
            Participant::agent(agent_id),
            None,
            seed.org_id,
            seed.user_id,
        )
        .await
        .expect("s2");
    let normal = NewPromptRequest::normal(
        None,
        Participant::Human,
        agent_id,
        None,
        Prompt::try_from("hello").expect("p"),
        IdempotencyKey::try_from("normal-k").expect("k"),
        seed.org_id,
        seed.user_id,
    );
    let outcome = queue.enqueue(normal).await.expect("enqueue normal");
    let claim = queue
        .claim_next_session(WorkerId::new())
        .await
        .expect("claim normal")
        .expect("normal claimed");
    assert_eq!(claim.kind_payload.kind(), RequestKind::Normal);
    assert_eq!(claim.session, outcome.session());

    // BUT: a second reflection on a different session for the same
    // agent must be skipped while the first reflection is still
    // processing. (Use s2 which is now bound to the same agent.)
    let _ = enqueue_reflection_row(
        &queue,
        outcome.session(),
        agent_id,
        PromptRequestId::new(),
        "k2",
        seed.org_id,
        seed.user_id,
    )
    .await;
    let second = queue
        .claim_next_session(WorkerId::new())
        .await
        .expect("claim again");
    assert!(
        second.is_none(),
        "second reflection on the same agent must wait, got {second:?}"
    );
}

#[sqlx::test]
async fn scheduler_tick_enqueues_for_idle_session_with_unprocessed_turns(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let clock: SharedClock = SystemClock::shared();
    let sessions: SharedSessionStore = Arc::new(PgSessionStore::new(pool.clone(), clock.clone()));
    let queue: SharedPromptQueue = Arc::new(PgPromptQueue::new(pool.clone(), clock.clone()));

    // Mint a session, append one message back-dated past the idle
    // threshold so the scheduler's "idle" predicate fires immediately.
    let session =
        human_to_agent_session(sessions.as_ref(), seed.agent_id, seed.org_id, seed.user_id).await;
    let request_id =
        common::pg::seed_prompt_request(&pool, session, seed.agent_id, seed.org_id).await;
    let stale_ts = Utc::now() - chrono::Duration::seconds(60 * 60);
    sqlx::query(
        "INSERT INTO session_messages
             (session_id, seq, request_id, body,
              sender_kind, sender_agent_id,
              receiver_kind, receiver_agent_id, created_at, org_id)
         VALUES ($1, 1, $2, $3,
                 'human', NULL,
                 'agent', $4, $5, $6)",
    )
    .bind(session)
    .bind(request_id)
    .bind(serde_json::json!({
        "role": "user",
        "contents": [{"kind": "text", "value": "old message"}]
    }))
    .bind(seed.agent_id)
    .bind(stale_ts)
    .bind(seed.org_id)
    .execute(&pool)
    .await
    .expect("seed message");

    // Spawn the scheduler with a tight poll cadence so the test does
    // not have to wait the production 60s. We only need one tick.
    let scheduler = ReflectionScheduler::spawn_with_cadence(
        pool.clone(),
        queue.clone(),
        clock,
        Duration::from_millis(100),
        None,
    );
    // Poll until the reflection row appears. The link from conversation to
    // reflection row is `kind_payload.data.session_id`, not
    // `prompt_requests.session_id` (which points at the reflection's own
    // off-conversation session).
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let mut found = None;
    while std::time::Instant::now() < deadline {
        let row: Option<(uuid::Uuid,)> = sqlx::query_as(
            "SELECT id FROM prompt_requests
             WHERE kind = 'reflection'
               AND kind_payload->'data'->>'session_id' = $1::text
             LIMIT 1",
        )
        .bind(session.as_uuid().to_string())
        .fetch_optional(&pool)
        .await
        .expect("query");
        if row.is_some() {
            found = row;
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    scheduler.shutdown().await;
    assert!(
        found.is_some(),
        "scheduler should have enqueued one reflection"
    );
}

#[sqlx::test]
async fn scheduler_does_not_duplicate_pending_reflection(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let clock: SharedClock = SystemClock::shared();
    let sessions: SharedSessionStore = Arc::new(PgSessionStore::new(pool.clone(), clock.clone()));
    let queue: SharedPromptQueue = Arc::new(PgPromptQueue::new(pool.clone(), clock));
    let session =
        human_to_agent_session(sessions.as_ref(), seed.agent_id, seed.org_id, seed.user_id).await;
    let since = PromptRequestId::new();

    // Pre-seed a reflection row in `pending`. The scheduler's predicate
    // (NOT EXISTS pending/processing reflection) should refuse to add
    // another for the same session.
    let _ = enqueue_reflection_row(
        &queue,
        session,
        seed.agent_id,
        since,
        "preseeded",
        seed.org_id,
        seed.user_id,
    )
    .await;
    let count_before: (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM prompt_requests
         WHERE session_id = $1 AND kind = 'reflection'",
    )
    .bind(session)
    .fetch_one(&pool)
    .await
    .expect("count");
    assert_eq!(count_before.0, 1);

    // Now back-date a message so the scheduler would otherwise enqueue
    // a fresh reflection. The dedup predicate must keep the count at 1.
    let request_id =
        common::pg::seed_prompt_request(&pool, session, seed.agent_id, seed.org_id).await;
    sqlx::query(
        "INSERT INTO session_messages
             (session_id, seq, request_id, body,
              sender_kind, sender_agent_id,
              receiver_kind, receiver_agent_id, created_at, org_id)
         VALUES ($1, 1, $2, $3,
                 'human', NULL,
                 'agent', $4, $5, $6)",
    )
    .bind(session)
    .bind(request_id)
    .bind(serde_json::json!({
        "role": "user",
        "contents": [{"kind": "text", "value": "older"}]
    }))
    .bind(seed.agent_id)
    .bind(Utc::now() - chrono::Duration::seconds(60 * 60))
    .bind(seed.org_id)
    .execute(&pool)
    .await
    .expect("seed message");

    let scheduler = ReflectionScheduler::spawn_with_cadence(
        pool.clone(),
        queue,
        SystemClock::shared(),
        Duration::from_millis(100),
        None,
    );
    tokio::time::sleep(Duration::from_secs(2)).await;
    scheduler.shutdown().await;

    let count_after: (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM prompt_requests
         WHERE session_id = $1 AND kind = 'reflection'",
    )
    .bind(session)
    .fetch_one(&pool)
    .await
    .expect("count after");
    assert_eq!(
        count_after.0, 1,
        "scheduler must not duplicate while a pending reflection exists"
    );
}

#[sqlx::test]
async fn scheduler_skips_session_whose_seed_turn_is_reflection(pool: PgPool) {
    // Regression: the scheduler used to find every agent-participant session
    // including the off-DAG sessions it had just minted for reflection jobs,
    // which produced reflections of reflections of reflections ad nauseam.
    // The fix filters candidates to sessions whose seed prompt_request has
    // `kind = 'normal'`.
    let seed = seed_tenant(&pool).await;
    let clock: SharedClock = SystemClock::shared();
    let sessions: SharedSessionStore = Arc::new(PgSessionStore::new(pool.clone(), clock.clone()));
    let queue: SharedPromptQueue = Arc::new(PgPromptQueue::new(pool.clone(), clock.clone()));

    let agent_id = seed.agent_id;
    let session =
        human_to_agent_session(sessions.as_ref(), agent_id, seed.org_id, seed.user_id).await;

    // Insert a seed prompt_request whose id matches the session's
    // root_request_id and whose kind is 'reflection' — i.e. the shape of
    // a freshly-minted reflection session in production.
    let root_id: uuid::Uuid =
        sqlx::query_scalar("SELECT root_request_id FROM sessions WHERE id = $1")
            .bind(session)
            .fetch_one(&pool)
            .await
            .expect("fetch root");
    let now = chrono::Utc::now();
    sqlx::query(
        "INSERT INTO prompt_requests
             (id, session_id, org_id, content, idempotency_key, status,
              sender_kind, sender_agent_id,
              receiver_kind, receiver_agent_id, root_request_id,
              kind, kind_payload, created_at, updated_at)
         VALUES ($1, $2, $7, '(reflection)', $3, 'done',
                 'agent', $4,
                 'agent', $4, $1,
                 'reflection', $5, $6, $6)",
    )
    .bind(root_id)
    .bind(session)
    .bind(format!("seed-reflection-{root_id}"))
    .bind(agent_id)
    .bind(serde_json::json!({
        "kind": "reflection",
        "data": { "session_id": session, "up_to_turn_id": root_id }
    }))
    .bind(now)
    .bind(seed.org_id)
    .execute(&pool)
    .await
    .expect("seed reflection-kind root request");

    // Append a back-dated message so the scheduler's idle predicate would
    // otherwise fire. The seed-kind filter must keep this session out.
    sqlx::query(
        "INSERT INTO session_messages
             (session_id, seq, request_id, body,
              sender_kind, sender_agent_id,
              receiver_kind, receiver_agent_id, created_at, org_id)
         VALUES ($1, 1, $2, $3,
                 'agent', $4,
                 'agent', $4, $5, $6)",
    )
    .bind(session)
    .bind(root_id)
    .bind(serde_json::json!({
        "role": "assistant",
        "contents": [{"kind": "text", "value": "prior reflection output"}]
    }))
    .bind(agent_id)
    .bind(Utc::now() - chrono::Duration::seconds(60 * 60))
    .bind(seed.org_id)
    .execute(&pool)
    .await
    .expect("seed message");

    let scheduler = ReflectionScheduler::spawn_with_cadence(
        pool.clone(),
        queue,
        clock,
        Duration::from_millis(100),
        None,
    );
    tokio::time::sleep(Duration::from_secs(2)).await;
    scheduler.shutdown().await;

    // Count reflections targeting the test session but excluding the seed
    // row we inserted ourselves — the scheduler-enqueued reflections live on
    // their own off-DAG (agent, system) session, so filtering on session_id
    // != test session is the cleanest separator.
    let scheduled_reflections: (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM prompt_requests
         WHERE kind = 'reflection'
           AND kind_payload->'data'->>'session_id' = $1::text
           AND session_id <> $1",
    )
    .bind(session)
    .fetch_one(&pool)
    .await
    .expect("count");
    assert_eq!(
        scheduled_reflections.0, 0,
        "scheduler must not enqueue a reflection on a reflection-rooted session"
    );
}
