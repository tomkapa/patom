//! Trait-contract tests for [`patom::scheduling::PgScheduledTaskStore`]:
//! create + list round-trip, ownership-checked cancel, claim_due filter
//! ordering, record_fired state advancement, and per-owner cap counting.
//!
//! Each test uses a fresh schema; tests use `SystemClock` since none of
//! these operations are time-sensitive (the scheduler-end-to-end test in
//! `scheduling_pipeline.rs` exercises the wall-clock path).

#![allow(clippy::expect_used)]

use std::sync::Arc;

use chrono::{Duration as ChronoDuration, TimeZone, Utc};
use chrono_tz::Asia::Bangkok;
use patom::agents::{AgentId, AgentName, AgentStore, AgentSystemPrompt, NewAgent};
use patom::clock::SystemClock;
use patom::runtime::PromptRequestId;
use patom::scheduling::{
    NewScheduledTask, PgScheduledTaskStore, ScheduleSpec, ScheduledPrompt, ScheduledTaskError,
    ScheduledTaskName, ScheduledTaskState, ScheduledTaskStore, TimeOfDay, Timezone, Weekday,
    Weekdays,
};
use sqlx::PgPool;

mod common;
use common::pg::seed_tenant;

fn store(pool: &PgPool) -> Arc<PgScheduledTaskStore> {
    Arc::new(PgScheduledTaskStore::new(
        pool.clone(),
        SystemClock::shared(),
    ))
}

/// Most contract tests use the seeded default agent/org/user, so the
/// helper threads them through `NewScheduledTask`. Tests that need a
/// different owner pass it explicitly via the longer-form constructor.
struct Tenancy {
    owner: AgentId,
    org_id: patom::auth::OrgId,
    user_id: patom::auth::UserId,
}

impl Tenancy {
    fn default_for(seed: &common::pg::Seed) -> Self {
        Self {
            owner: seed.agent_id,
            org_id: seed.org_id,
            user_id: seed.user_id,
        }
    }

    fn with_owner(seed: &common::pg::Seed, owner: AgentId) -> Self {
        Self {
            owner,
            org_id: seed.org_id,
            user_id: seed.user_id,
        }
    }
}

async fn extra_agent(pool: &PgPool, seed: &common::pg::Seed, name: &str) -> AgentId {
    let agents = common::pg::agent_store(pool.clone(), SystemClock::shared());
    let payload = NewAgent {
        org_id: seed.org_id,
        name: AgentName::try_from(name).expect("valid name"),
        system_prompt: AgentSystemPrompt::try_from("p").expect("valid prompt"),
        description: patom::agents::AgentDescription::try_from("p").expect("desc"),
        allowed_mcp_tools: patom::agents::AllowedMcpTools::empty(),
        model: None,
        avatar_url: None,
        edited_by: None,
    };
    agents.create(payload).await.expect("create agent").id
}

fn once_at(year: i32, month: u32, day: u32, hour: u32) -> ScheduleSpec {
    ScheduleSpec::Once {
        run_at: Utc
            .with_ymd_and_hms(year, month, day, hour, 0, 0)
            .single()
            .expect("unambiguous"),
    }
}

fn recurring_workdays_05_bkk() -> ScheduleSpec {
    ScheduleSpec::Recurring {
        weekdays: Weekdays::WORKDAYS,
        time: TimeOfDay::try_new(5, 0).expect("HH:MM"),
        tz: Timezone::from_tz(Bangkok),
    }
}

fn new_task(t: &Tenancy, name: &str, schedule: ScheduleSpec) -> NewScheduledTask {
    let next = schedule.next_after(Utc::now());
    NewScheduledTask {
        owner_agent_id: t.owner,
        org_id: t.org_id,
        created_by_user_id: t.user_id,
        channel_id: None,
        name: ScheduledTaskName::try_from(name).expect("valid name"),
        prompt: ScheduledPrompt::try_from("Summarize new email since last check.")
            .expect("valid prompt"),
        schedule,
        next_run_at: next,
    }
}

#[sqlx::test]
async fn create_round_trips_once_schedule(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let store = store(&pool);

    let t = Tenancy::default_for(&seed);
    let payload = new_task(&t, "drafts due tomorrow", once_at(2030, 1, 1, 9));
    let row = store.create(payload).await.expect("create");

    assert_eq!(row.owner_agent_id, seed.agent_id);
    assert_eq!(row.org_id, seed.org_id);
    assert_eq!(row.created_by_user_id, seed.user_id);
    assert_eq!(row.name.as_str(), "drafts due tomorrow");
    assert_eq!(row.state, ScheduledTaskState::Active);
    assert!(row.next_run_at.is_some(), "Once in future has next_run_at");
    assert!(row.last_fired_at.is_none());
    assert!(row.last_request_id.is_none());
    match row.schedule {
        ScheduleSpec::Once { run_at } => {
            assert_eq!(run_at.timestamp(), 1_893_488_400); // 2030-01-01T09:00Z
        }
        ScheduleSpec::Recurring { .. } => panic!("expected Once, got Recurring"),
    }
}

#[sqlx::test]
async fn create_round_trips_recurring_schedule(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let store = store(&pool);

    let t = Tenancy::default_for(&seed);
    let payload = new_task(&t, "morning email", recurring_workdays_05_bkk());
    let row = store.create(payload).await.expect("create");

    match row.schedule {
        ScheduleSpec::Recurring { weekdays, time, tz } => {
            assert_eq!(weekdays.bits(), Weekdays::WORKDAYS.bits());
            assert_eq!(time.hour(), 5);
            assert_eq!(time.minute(), 0);
            assert_eq!(tz.name(), "Asia/Bangkok");
        }
        ScheduleSpec::Once { .. } => panic!("expected Recurring, got Once"),
    }
}

#[sqlx::test]
async fn list_for_agent_returns_only_own_active_rows(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let store = store(&pool);
    let other = extra_agent(&pool, &seed, "other-agent").await;
    let default_t = Tenancy::default_for(&seed);
    let other_t = Tenancy::with_owner(&seed, other);

    // Two tasks for the default agent, one for the other agent.
    let mine_a = store
        .create(new_task(&default_t, "mine-a", once_at(2030, 1, 1, 9)))
        .await
        .expect("ok")
        .id;
    let mine_b = store
        .create(new_task(&default_t, "mine-b", recurring_workdays_05_bkk()))
        .await
        .expect("ok")
        .id;
    let _theirs = store
        .create(new_task(&other_t, "theirs", once_at(2030, 1, 1, 9)))
        .await
        .expect("ok")
        .id;

    let rows = store.list_for_agent(seed.agent_id).await.expect("list");
    let ids: Vec<_> = rows.iter().map(|r| r.id).collect();
    assert_eq!(rows.len(), 2);
    assert!(ids.contains(&mine_a));
    assert!(ids.contains(&mine_b));
}

#[sqlx::test]
async fn list_excludes_cancelled_rows(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let store = store(&pool);

    let t = Tenancy::default_for(&seed);
    let kept = store
        .create(new_task(&t, "kept", once_at(2030, 1, 1, 9)))
        .await
        .expect("ok")
        .id;
    let dropped = store
        .create(new_task(&t, "drop", once_at(2030, 1, 1, 9)))
        .await
        .expect("ok")
        .id;

    store.cancel(dropped, seed.agent_id).await.expect("cancel");

    let rows = store.list_for_agent(seed.agent_id).await.expect("list");
    let ids: Vec<_> = rows.iter().map(|r| r.id).collect();
    assert_eq!(ids, vec![kept]);
}

#[sqlx::test]
async fn cancel_rejects_cross_owner(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let store = store(&pool);
    let other = extra_agent(&pool, &seed, "intruder").await;

    let t = Tenancy::default_for(&seed);
    let task = store
        .create(new_task(&t, "t", once_at(2030, 1, 1, 9)))
        .await
        .expect("ok")
        .id;

    // Cross-owner attempts fold into NotFound so the tool seam cannot
    // be used to probe for other agents' rows.
    let err = store.cancel(task, other).await.expect_err("not owner");
    assert!(matches!(err, ScheduledTaskError::NotFound(_)));

    // Original row still active — the failed cancel must not have flipped state.
    let rows = store.list_for_agent(seed.agent_id).await.expect("list");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].state, ScheduledTaskState::Active);
}

#[sqlx::test]
async fn cancel_returns_not_found_for_missing_id(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let store = store(&pool);

    let phantom = patom::scheduling::ScheduledTaskId::new();
    let err = store
        .cancel(phantom, seed.agent_id)
        .await
        .expect_err("missing");
    assert!(matches!(err, ScheduledTaskError::NotFound(_)));
}

#[sqlx::test]
async fn cancel_is_idempotent_on_already_cancelled(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let store = store(&pool);

    let t = Tenancy::default_for(&seed);
    let id = store
        .create(new_task(&t, "t", once_at(2030, 1, 1, 9)))
        .await
        .expect("ok")
        .id;
    store.cancel(id, seed.agent_id).await.expect("first");
    // Second call against an already-cancelled row is a no-op (Ok).
    store.cancel(id, seed.agent_id).await.expect("second");
}

#[sqlx::test]
async fn claim_due_returns_only_due_active_rows_in_order(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let store = store(&pool);

    let now = Utc::now();
    let earliest_due = now - ChronoDuration::seconds(120);
    let later_due = now - ChronoDuration::seconds(30);
    let future = now + ChronoDuration::days(7);

    let t = Tenancy::default_for(&seed);
    // Three rows: earliest due, later due, far-future (not due).
    let early = insert_with_next_run(&store, &t, "early", earliest_due).await;
    let later = insert_with_next_run(&store, &t, "later", later_due).await;
    let _far = insert_with_next_run(&store, &t, "far", future).await;

    // Cancelled row that is "due" — must be excluded.
    let cancelled = insert_with_next_run(&store, &t, "cxl", earliest_due).await;
    store
        .cancel(cancelled, seed.agent_id)
        .await
        .expect("cancel");

    let claimed = store.claim_due(now, 10).await.expect("claim");
    let ids: Vec<_> = claimed.iter().map(|r| r.id).collect();
    assert_eq!(ids, vec![early, later]);
}

#[sqlx::test]
async fn claim_due_respects_limit(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let store = store(&pool);

    let t = Tenancy::default_for(&seed);
    let now = Utc::now();
    let due = now - ChronoDuration::seconds(60);
    for i in 0..5 {
        insert_with_next_run(&store, &t, &format!("t-{i}"), due).await;
    }

    let claimed = store.claim_due(now, 2).await.expect("claim");
    assert_eq!(claimed.len(), 2);
}

#[sqlx::test]
async fn record_fired_advances_when_next_is_some(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let store = store(&pool);

    let t = Tenancy::default_for(&seed);
    let now = Utc::now();
    let id = insert_with_next_run(&store, &t, "advance", now - ChronoDuration::seconds(10)).await;
    let request_id = PromptRequestId::new();
    let next = now + ChronoDuration::days(1);

    store
        .record_fired(id, request_id, now, Some(next))
        .await
        .expect("record_fired");

    let row = store
        .list_for_agent(seed.agent_id)
        .await
        .expect("list")
        .into_iter()
        .find(|r| r.id == id)
        .expect("present");
    assert_eq!(row.state, ScheduledTaskState::Active);
    assert_eq!(row.last_request_id, Some(request_id));
    assert!(row.last_fired_at.is_some());
    assert_eq!(
        row.next_run_at.map(|t| t.timestamp()),
        Some(next.timestamp())
    );
}

#[sqlx::test]
async fn record_fired_marks_done_when_next_is_none(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let store = store(&pool);

    let t = Tenancy::default_for(&seed);
    let now = Utc::now();
    let id = insert_with_next_run(&store, &t, "exhausted", now - ChronoDuration::seconds(10)).await;
    let request_id = PromptRequestId::new();

    store
        .record_fired(id, request_id, now, None)
        .await
        .expect("record_fired");

    // list_for_agent filters to active rows only — Done row must not appear.
    let rows = store.list_for_agent(seed.agent_id).await.expect("list");
    assert!(rows.iter().all(|r| r.id != id));

    // And the row must not be claimable any more.
    let later = now + ChronoDuration::days(365);
    let claimed = store.claim_due(later, 100).await.expect("claim");
    assert!(claimed.iter().all(|r| r.id != id));
}

/// Insert a task with an explicit `next_run_at` so claim_due / record_fired
/// tests don't rely on `ScheduleSpec::next_after`'s wall-clock.
async fn insert_with_next_run(
    store: &Arc<PgScheduledTaskStore>,
    t: &Tenancy,
    name: &str,
    next_run_at: chrono::DateTime<Utc>,
) -> patom::scheduling::ScheduledTaskId {
    let payload = NewScheduledTask {
        owner_agent_id: t.owner,
        org_id: t.org_id,
        created_by_user_id: t.user_id,
        channel_id: None,
        name: ScheduledTaskName::try_from(name).expect("valid"),
        prompt: ScheduledPrompt::try_from("body").expect("valid"),
        // Recurring with a single weekday so the row is reusable across fires
        // — content of `schedule` doesn't drive these tests.
        schedule: ScheduleSpec::Recurring {
            weekdays: Weekdays::try_from_iter([Weekday::Mon]).expect("non-empty"),
            time: TimeOfDay::try_new(5, 0).expect("HH:MM"),
            tz: Timezone::from_tz(Bangkok),
        },
        next_run_at: Some(next_run_at),
    };
    store.create(payload).await.expect("create").id
}
