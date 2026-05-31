//! End-to-end test for the scheduled-task pipeline:
//!
//! 1. Insert a `Once` task whose `next_run_at` is already in the past.
//! 2. Spawn the [`ScheduledTaskScheduler`] with a tight poll cadence.
//! 3. Wait for the scheduler to enqueue a `prompt_requests` row.
//! 4. Assert: the row landed with the right shape and the task's state
//!    advanced to `Done` (Once schedule, no further fires).
//!
//! A second test exercises the `Recurring` advance: after one fire the
//! task's `next_run_at` is moved forward and the row stays `Active`.

#![allow(clippy::expect_used)]

use std::sync::Arc;
use std::time::Duration;

use chrono::{Duration as ChronoDuration, TimeZone, Utc};
use chrono_tz::Asia::Bangkok;
use patom::clock::{SharedClock, SystemClock};
use patom::runtime::{
    IdempotencyKey, NewPromptRequest, PgPromptQueue, RequestKind, RequestStatus, SharedPromptQueue,
};
use patom::scheduling::{
    NewScheduledTask, PgScheduledTaskStore, ScheduleSpec, ScheduledPrompt, ScheduledTaskId,
    ScheduledTaskName, ScheduledTaskScheduler, ScheduledTaskState, SharedScheduledTaskStore,
    TimeOfDay, Timezone, Weekdays,
};
use patom::types::{Participant, Prompt};
use sqlx::PgPool;

mod common;
use common::pg::seed_tenant;

struct Fixture {
    pool: PgPool,
    store: SharedScheduledTaskStore,
    queue: SharedPromptQueue,
    clock: SharedClock,
    default_agent_id: patom::agents::AgentId,
    default_org_id: patom::auth::OrgId,
    default_user_id: patom::auth::UserId,
}

async fn fresh(pool: PgPool) -> Fixture {
    let seed = seed_tenant(&pool).await;
    let clock: SharedClock = SystemClock::shared();
    let store: SharedScheduledTaskStore =
        Arc::new(PgScheduledTaskStore::new(pool.clone(), clock.clone()));
    let queue: SharedPromptQueue = Arc::new(PgPromptQueue::new(pool.clone(), clock.clone()));
    Fixture {
        pool,
        store,
        queue,
        clock,
        default_agent_id: seed.agent_id,
        default_org_id: seed.org_id,
        default_user_id: seed.user_id,
    }
}

/// Look up the row state directly via SQL — `list_for_agent` filters to
/// active rows so a `Done` task wouldn't appear there.
async fn read_state(pool: &sqlx::PgPool, id: ScheduledTaskId) -> ScheduledTaskState {
    let (raw,): (String,) = sqlx::query_as("SELECT state FROM scheduled_tasks WHERE id = $1")
        .bind(id)
        .fetch_one(pool)
        .await
        .expect("read state");
    ScheduledTaskState::parse(&raw).expect("known state")
}

#[sqlx::test]
async fn scheduler_fires_due_once_task_and_marks_done(pool: PgPool) {
    let f = fresh(pool).await;

    // Once schedule whose next_run_at is 30s in the past — the scheduler
    // should pick it up on the first tick.
    let due_at = Utc::now() - ChronoDuration::seconds(30);
    let payload = NewScheduledTask {
        owner_agent_id: f.default_agent_id,
        org_id: f.default_org_id,
        created_by_user_id: f.default_user_id,
        name: ScheduledTaskName::try_from("draft tomorrow's brief").expect("name"),
        prompt: ScheduledPrompt::try_from("Draft tomorrow's morning brief.").expect("prompt"),
        schedule: ScheduleSpec::Once {
            // run_at is in the past so next_after returns None — the
            // scheduler will record_fired with next=None and mark Done.
            run_at: Utc
                .with_ymd_and_hms(2020, 1, 1, 9, 0, 0)
                .single()
                .expect("unambiguous"),
        },
        next_run_at: Some(due_at),
    };
    let task_id = f.store.create(payload).await.expect("create").id;

    let scheduler = ScheduledTaskScheduler::spawn_with_cadence(
        f.store.clone(),
        f.queue.clone(),
        f.clock.clone(),
        Duration::from_millis(50),
        None,
    );

    // Poll for the prompt row to appear. The idempotency key is
    // `sched-{task_id}-{fire_ts}` which we can match exactly.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let mut request_row: Option<(uuid::Uuid, String, String)> = None;
    while std::time::Instant::now() < deadline {
        let row: Option<(uuid::Uuid, String, String)> = sqlx::query_as(
            "SELECT id, idempotency_key, kind FROM prompt_requests \
             WHERE idempotency_key LIKE $1 \
             LIMIT 1",
        )
        .bind(format!("sched-{task_id}-%"))
        .fetch_optional(&f.pool)
        .await
        .expect("poll");
        if row.is_some() {
            request_row = row;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    scheduler.shutdown().await;

    let (_request_id, key, kind) = request_row.expect("scheduler enqueued one row");
    assert!(
        key.starts_with(&format!("sched-{task_id}-")),
        "key shape: {key}",
    );
    assert_eq!(kind, RequestKind::Normal.as_str(), "fires as Normal");

    // Once schedule with no future fire ⇒ task transitions to Done.
    let state = read_state(&f.pool, task_id).await;
    assert_eq!(state, ScheduledTaskState::Done);

    // And its sender_kind is `human` (the scheduler enqueues as if a
    // human had submitted the prompt — see app.rs system-prompt point 9).
    let (sender_kind, receiver_agent_id, status): (String, uuid::Uuid, String) = sqlx::query_as(
        "SELECT sender_kind, receiver_agent_id, status FROM prompt_requests \
         WHERE idempotency_key LIKE $1 LIMIT 1",
    )
    .bind(format!("sched-{task_id}-%"))
    .fetch_one(&f.pool)
    .await
    .expect("re-read row");
    assert_eq!(sender_kind, "human");
    assert_eq!(receiver_agent_id, f.default_agent_id.as_uuid());
    assert_eq!(status, RequestStatus::Pending.as_str());
}

#[sqlx::test]
async fn scheduler_advances_recurring_task_after_fire(pool: PgPool) {
    let f = fresh(pool).await;

    // Recurring schedule due now, with a real next_after past `now`.
    // Use ALL weekdays so the next fire is always exactly tomorrow at
    // 05:00 BKK — keeps the assertion deterministic regardless of the
    // weekday "now" lands on.
    let due_at = Utc::now() - ChronoDuration::seconds(30);
    let payload = NewScheduledTask {
        owner_agent_id: f.default_agent_id,
        org_id: f.default_org_id,
        created_by_user_id: f.default_user_id,
        name: ScheduledTaskName::try_from("morning email").expect("name"),
        prompt: ScheduledPrompt::try_from("Summarize new email.").expect("prompt"),
        schedule: ScheduleSpec::Recurring {
            weekdays: Weekdays::ALL,
            time: TimeOfDay::try_new(5, 0).expect("HH:MM"),
            tz: Timezone::from_tz(Bangkok),
        },
        next_run_at: Some(due_at),
    };
    let task_id = f.store.create(payload).await.expect("create").id;

    let scheduler = ScheduledTaskScheduler::spawn_with_cadence(
        f.store.clone(),
        f.queue.clone(),
        f.clock.clone(),
        Duration::from_millis(50),
        None,
    );

    // Poll until next_run_at advances past the original due_at.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let mut row: Option<(chrono::DateTime<Utc>, String)> = None;
    while std::time::Instant::now() < deadline {
        let probe: Option<(Option<chrono::DateTime<Utc>>, String)> =
            sqlx::query_as("SELECT next_run_at, state FROM scheduled_tasks WHERE id = $1")
                .bind(task_id)
                .fetch_optional(&f.pool)
                .await
                .expect("poll");
        if let Some((Some(next), state)) = probe
            && next > due_at
        {
            row = Some((next, state));
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    scheduler.shutdown().await;

    let (next_after_fire, state) = row.expect("scheduler advanced next_run_at");
    assert!(
        next_after_fire > due_at,
        "next_run_at moved forward: {next_after_fire} > {due_at}",
    );
    // Recurring task with future fires ⇒ state stays Active.
    assert_eq!(state, ScheduledTaskState::Active.as_str());

    // last_fired_at + last_request_id were stamped.
    let (last_fired, last_req): (Option<chrono::DateTime<Utc>>, Option<uuid::Uuid>) =
        sqlx::query_as("SELECT last_fired_at, last_request_id FROM scheduled_tasks WHERE id = $1")
            .bind(task_id)
            .fetch_one(&f.pool)
            .await
            .expect("re-read");
    assert!(last_fired.is_some(), "last_fired_at stamped");
    assert!(last_req.is_some(), "last_request_id stamped");
}

#[sqlx::test]
async fn scheduler_idempotent_on_repeated_ticks_for_same_fire(pool: PgPool) {
    // A Recurring task that hasn't been advanced yet is still due on the
    // next tick. The queue's idempotency dedup
    // (`sched-{task_id}-{fire_ts}`) means repeated enqueues of the same
    // fire-instant collapse to one row. Verify by claiming twice in a
    // row before the scheduler had time to update next_run_at.
    let f = fresh(pool).await;

    let due_at = Utc::now() - ChronoDuration::seconds(30);
    let payload = NewScheduledTask {
        owner_agent_id: f.default_agent_id,
        org_id: f.default_org_id,
        created_by_user_id: f.default_user_id,
        name: ScheduledTaskName::try_from("idempotent").expect("name"),
        prompt: ScheduledPrompt::try_from("body").expect("prompt"),
        schedule: ScheduleSpec::Recurring {
            weekdays: Weekdays::ALL,
            time: TimeOfDay::try_new(5, 0).expect("HH:MM"),
            tz: Timezone::from_tz(Bangkok),
        },
        next_run_at: Some(due_at),
    };
    let task_id = f.store.create(payload).await.expect("create").id;

    // Manually run two claim-and-fire rounds with the same fire_at —
    // the second should hit `EnqueueOutcome::Existing` and dedup.
    let due = f.store.claim_due(due_at, 10).await.expect("claim 1");
    assert_eq!(due.len(), 1);
    let task = &due[0];
    let fire_at = task.next_run_at.expect("populated");

    let key_str = format!("sched-{task_id}-{}", fire_at.timestamp());
    let key1 = IdempotencyKey::try_from(key_str.clone()).expect("key1");
    let key2 = IdempotencyKey::try_from(key_str).expect("key2");

    let make_req = |k: IdempotencyKey| {
        NewPromptRequest::normal(
            None,
            Participant::Human,
            f.default_agent_id,
            None,
            Prompt::try_from(task.prompt.as_str().to_string()).expect("p"),
            k,
            f.default_org_id,
            f.default_user_id,
        )
    };
    let first = f.queue.enqueue(make_req(key1)).await.expect("first");
    let second = f.queue.enqueue(make_req(key2)).await.expect("second");
    assert_eq!(
        first.request_id(),
        second.request_id(),
        "same idempotency key collapses to one row",
    );

    // Exactly one prompt_requests row for this task's idempotency prefix.
    let (count,): (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM prompt_requests WHERE idempotency_key LIKE $1")
            .bind(format!("sched-{task_id}-%"))
            .fetch_one(&f.pool)
            .await
            .expect("count");
    assert_eq!(count, 1);
}
