//! End-to-end probe for the scheduling-tenancy retrofit (migration 19).
//!
//! Scheduled tasks have no direct HTTP surface — agents create them via
//! the `schedule_task` system tool — so this probe targets the store
//! and scheduler directly. Three contracts mirror the spirit of the
//! prior probes (`tests/auth_mcp_servers.rs`, `tests/auth_agents.rs`,
//! `tests/auth_threads.rs`, `tests/auth_memory.rs`):
//!
//!   1. Per-owner store visibility: two orgs each schedule one task;
//!      `list_for_agent` for each owner returns only that owner's row.
//!   2. DB-level parity invariant: the trigger on `scheduled_tasks`
//!      rejects an INSERT whose `org_id` doesn't match the owning
//!      agent's `agents.org_id`.
//!   3. Privileged scheduler scan + per-row tenancy: `claim_due`
//!      returns rows for both orgs in one tick, and the enqueued
//!      `prompt_requests` rows each carry their owning task's `org_id`
//!      verbatim.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::sync::Arc;
use std::time::Duration;

use chrono::{Duration as ChronoDuration, Utc};
use chrono_tz::Asia::Bangkok;
use patom::agents::{
    AgentDescription, AgentId, AgentName, AgentSystemPrompt, AllowedMcpTools, NewAgent,
    SharedAgentStore,
};
use patom::auth::{OrgId, UserId, begin_privileged};
use patom::clock::SystemClock;
use patom::runtime::{PgPromptQueue, SharedPromptQueue};
use patom::scheduling::{
    NewScheduledTask, PgScheduledTaskStore, ScheduleSpec, ScheduledPrompt, ScheduledTaskId,
    ScheduledTaskName, ScheduledTaskScheduler, SharedScheduledTaskStore, TimeOfDay, Timezone,
    Weekdays,
};
use patom::threads::{PgThreadStore, SharedThreadStore};
use sqlx::PgPool;

mod common;
use common::auth::seed_principal;
use common::pg::seed_tenant;

struct AuthSchedHarness {
    pool: PgPool,
    agents: SharedAgentStore,
    store: SharedScheduledTaskStore,
    queue: SharedPromptQueue,
    threads: SharedThreadStore,
    colleagues: patom::colleagues::SharedColleagueStore,
    clock: patom::clock::SharedClock,
    default_agent_id: AgentId,
    default_org_id: OrgId,
    default_user_id: UserId,
}

impl AuthSchedHarness {
    async fn new(pool: &PgPool) -> Self {
        let seed = seed_tenant(pool).await;
        let clock = SystemClock::shared();
        let agents = common::pg::shared_agent_store(pool.clone(), clock.clone());
        let store: SharedScheduledTaskStore =
            Arc::new(PgScheduledTaskStore::new(pool.clone(), clock.clone()));
        let queue: SharedPromptQueue = Arc::new(PgPromptQueue::new(pool.clone(), clock.clone()));
        let threads: SharedThreadStore = Arc::new(PgThreadStore::new(pool.clone(), clock.clone()));
        let colleagues: patom::colleagues::SharedColleagueStore =
            Arc::new(patom::colleagues::PgColleagueStore::new(pool.clone()));
        Self {
            pool: pool.clone(),
            agents,
            store,
            queue,
            threads,
            colleagues,
            clock,
            default_agent_id: seed.agent_id,
            default_org_id: seed.org_id,
            default_user_id: seed.user_id,
        }
    }

    /// Mint a fresh agent under `org_id`. The seeded default agent in
    /// `default_org_id` is the "primary" tenant's agent; tests that
    /// need a second org seed it via `seed_principal` and create an
    /// agent under it through this helper.
    async fn fresh_agent(&self, org_id: OrgId, name: &str) -> AgentId {
        self.agents
            .create(NewAgent {
                org_id,
                name: AgentName::try_from(name).expect("name"),
                system_prompt: AgentSystemPrompt::try_from("scoped prompt").expect("prompt"),
                description: AgentDescription::try_from(format!("agent {name}")).expect("desc"),
                allowed_mcp_tools: AllowedMcpTools::empty(),
                model: None,
                avatar_url: None,
                edited_by: None,
            })
            .await
            .expect("create agent")
            .id
    }

    fn build_task(
        owner: AgentId,
        org_id: OrgId,
        user_id: UserId,
        name: &str,
        due_at: chrono::DateTime<Utc>,
    ) -> NewScheduledTask {
        NewScheduledTask {
            owner_agent_id: owner,
            org_id,
            created_by_user_id: user_id,
            channel_id: None,
            name: ScheduledTaskName::try_from(name).expect("name"),
            prompt: ScheduledPrompt::try_from("body").expect("prompt"),
            schedule: ScheduleSpec::Recurring {
                weekdays: Weekdays::ALL,
                time: TimeOfDay::try_new(5, 0).expect("HH:MM"),
                tz: Timezone::from_tz(Bangkok),
            },
            next_run_at: Some(due_at),
        }
    }
}

#[sqlx::test]
async fn list_for_agent_is_per_owner_across_orgs(pool: PgPool) {
    let h = AuthSchedHarness::new(&pool).await;

    // Second org + agent (the seeded `default_org_id` is the first).
    let other = seed_principal(&h.pool, &common::auth::test_jwt(h.clock.clone())).await;
    let other_agent = h.fresh_agent(other.org_id, "beta").await;

    // One task per agent.
    let due_at = Utc::now() + ChronoDuration::days(1);
    let primary_task = h
        .store
        .create(AuthSchedHarness::build_task(
            h.default_agent_id,
            h.default_org_id,
            h.default_user_id,
            "primary",
            due_at,
        ))
        .await
        .expect("primary create")
        .id;
    let other_task = h
        .store
        .create(AuthSchedHarness::build_task(
            other_agent,
            other.org_id,
            other.user_id,
            "other",
            due_at,
        ))
        .await
        .expect("other create")
        .id;

    // Primary owner sees only their task.
    let primary_rows = h
        .store
        .list_for_agent(h.default_agent_id)
        .await
        .expect("list primary");
    let primary_ids: Vec<_> = primary_rows.iter().map(|r| r.id).collect();
    assert_eq!(primary_ids, vec![primary_task]);
    // Round-trip tenancy off the row.
    assert_eq!(primary_rows[0].org_id, h.default_org_id);
    assert_eq!(primary_rows[0].created_by_user_id, h.default_user_id);

    // Other owner sees only their task.
    let other_rows = h
        .store
        .list_for_agent(other_agent)
        .await
        .expect("list other");
    let other_ids: Vec<_> = other_rows.iter().map(|r| r.id).collect();
    assert_eq!(other_ids, vec![other_task]);
    assert_eq!(other_rows[0].org_id, other.org_id);
    assert_eq!(other_rows[0].created_by_user_id, other.user_id);
}

#[sqlx::test]
async fn parity_trigger_rejects_mismatched_org_id(pool: PgPool) {
    // The `scheduled_tasks_enforce_org` trigger (migration 19) raises
    // when `scheduled_tasks.org_id` does not match the owning agent's
    // `agents.org_id`. Issue a raw INSERT under a privileged tx (RLS
    // would otherwise hide the cross-org row before the trigger fires)
    // and assert the error surface contains the trigger's message.
    let h = AuthSchedHarness::new(&pool).await;
    let other = seed_principal(&h.pool, &common::auth::test_jwt(h.clock.clone())).await;

    // Owning agent is in `default_org_id`; we try to pin the task to
    // `other.org_id` — the trigger must reject.
    let mut tx = begin_privileged(&h.pool).await.expect("begin");
    let now = Utc::now();
    let res = sqlx::query(
        "INSERT INTO scheduled_tasks
             (id, owner_agent_id, org_id, created_by_user_id,
              name, prompt, schedule, next_run_at, state,
              created_at, updated_at)
         VALUES ($1, $2, $3, $4, $5, $6, $7::jsonb, $8, 'active', $9, $9)",
    )
    .bind(ScheduledTaskId::new())
    .bind(h.default_agent_id)
    .bind(other.org_id) // <- WRONG: doesn't match agent's org
    .bind(h.default_user_id)
    .bind("mismatch")
    .bind("body")
    .bind(serde_json::json!({
        "kind": "once",
        "data": { "run_at": "2030-01-01T09:00:00Z" }
    }))
    .bind(now + ChronoDuration::days(1))
    .bind(now)
    .execute(&mut *tx)
    .await;
    tx.rollback().await.expect("rollback");

    let err = res.expect_err("trigger should reject");
    let msg = err.to_string();
    assert!(
        msg.contains("does not match parent agent"),
        "expected parity trigger error, got: {msg}"
    );
}

#[sqlx::test]
async fn scheduler_fires_both_orgs_in_one_privileged_tick(pool: PgPool) {
    // Two due tasks in two different orgs. The scheduler scans
    // cross-tenant via `begin_privileged`, then enqueues each
    // `prompt_requests` row with the task's stored `org_id`. After
    // shutdown, both rows must be present in `prompt_requests` with
    // their owning task's `org_id` verbatim — and no row may carry the
    // wrong org (cross-tenant smear).
    let h = AuthSchedHarness::new(&pool).await;
    let other = seed_principal(&h.pool, &common::auth::test_jwt(h.clock.clone())).await;
    let other_agent = h.fresh_agent(other.org_id, "beta").await;

    let due_at = Utc::now() - ChronoDuration::seconds(30);
    let primary_task = h
        .store
        .create(AuthSchedHarness::build_task(
            h.default_agent_id,
            h.default_org_id,
            h.default_user_id,
            "primary-due",
            due_at,
        ))
        .await
        .expect("primary create")
        .id;
    let other_task = h
        .store
        .create(AuthSchedHarness::build_task(
            other_agent,
            other.org_id,
            other.user_id,
            "other-due",
            due_at,
        ))
        .await
        .expect("other create")
        .id;

    let scheduler = ScheduledTaskScheduler::spawn_with_cadence(
        h.store.clone(),
        h.queue.clone(),
        h.threads.clone(),
        h.colleagues.clone(),
        h.clock.clone(),
        Duration::from_millis(50),
        None,
    );

    // Wait for both prompt_requests rows to appear (one per task). The
    // idempotency key is `sched-{task_id}-{fire_ts}` so we can filter
    // exactly to scheduled rows.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let mut primary_org: Option<uuid::Uuid> = None;
    let mut other_org: Option<uuid::Uuid> = None;
    while std::time::Instant::now() < deadline {
        let rows: Vec<(String, uuid::Uuid)> = sqlx::query_as(
            "SELECT idempotency_key, org_id FROM prompt_requests \
             WHERE idempotency_key LIKE 'sched-%'",
        )
        .fetch_all(&h.pool)
        .await
        .expect("poll");
        for (key, org_id) in rows {
            if key.starts_with(&format!("sched-{primary_task}-")) {
                primary_org = Some(org_id);
            } else if key.starts_with(&format!("sched-{other_task}-")) {
                other_org = Some(org_id);
            }
        }
        if primary_org.is_some() && other_org.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    scheduler.shutdown().await;

    assert_eq!(
        primary_org,
        Some(h.default_org_id.as_uuid()),
        "primary task enqueued under its own org"
    );
    assert_eq!(
        other_org,
        Some(other.org_id.as_uuid()),
        "other task enqueued under its own org"
    );
}
