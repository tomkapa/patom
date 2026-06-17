//! Stage D (issue #178): the scheduler ensures outbound delivery of a fired
//! thread.
//!
//! Regression guard for "scheduled task ran, nothing on Lark/Discord": when a
//! due task fires, the scheduler must call `OutboundRouter::ensure_delivery` for
//! the thread it created (so the surface pump attaches). Before the fix the
//! scheduler had no router and made zero such calls. This drives the scheduler
//! directly with a recording fake router — no worker pool, no surface needed.

#![allow(clippy::expect_used)]

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use chrono::{Duration as ChronoDuration, TimeZone, Utc};

use patom::auth::OrgId;
use patom::clock::SystemClock;
use patom::colleagues::{PgColleagueStore, SharedColleagueStore};
use patom::outbound::{OutboundError, OutboundRouter};
use patom::runtime::{PgPromptQueue, SharedPromptQueue};
use patom::scheduling::{
    NewScheduledTask, PgScheduledTaskStore, ScheduleSpec, ScheduledPrompt, ScheduledTaskName,
    ScheduledTaskScheduler, SharedScheduledTaskStore,
};
use patom::threads::{PgThreadStore, SharedThreadStore, ThreadId};
use sqlx::PgPool;

mod common;
use common::pg::seed_tenant;

#[derive(Debug, Default)]
struct RecordingRouter {
    calls: Mutex<Vec<(OrgId, ThreadId)>>,
}

#[async_trait]
impl OutboundRouter for RecordingRouter {
    async fn ensure_delivery(&self, org: OrgId, thread: ThreadId) -> Result<(), OutboundError> {
        self.calls.lock().expect("mutex").push((org, thread));
        Ok(())
    }
}

#[sqlx::test]
async fn fire_ensures_outbound_delivery_for_the_fired_thread(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let clock = SystemClock::shared();

    // `#general` is auto-created with the owner enrolled.
    let (general,): (uuid::Uuid,) =
        sqlx::query_as("SELECT id FROM channels WHERE org_id = $1 AND name = 'general'")
            .bind(seed.org_id)
            .fetch_one(&pool)
            .await
            .expect("general channel");
    let general = patom::channels::ChannelId::from(general);

    let store: SharedScheduledTaskStore =
        Arc::new(PgScheduledTaskStore::new(pool.clone(), clock.clone()));
    let queue: SharedPromptQueue = Arc::new(PgPromptQueue::new(pool.clone(), clock.clone()));
    let threads: SharedThreadStore = Arc::new(PgThreadStore::new(pool.clone(), clock.clone()));
    let colleagues: SharedColleagueStore = Arc::new(PgColleagueStore::new(pool.clone()));

    // A `Once` task due 30s ago, targeting `#general`.
    store
        .create(NewScheduledTask {
            owner_agent_id: seed.agent_id,
            org_id: seed.org_id,
            created_by_user_id: seed.user_id,
            channel_id: Some(general),
            name: ScheduledTaskName::try_from("brief").expect("name"),
            prompt: ScheduledPrompt::try_from("Post the brief.").expect("prompt"),
            schedule: ScheduleSpec::Once {
                run_at: Utc
                    .with_ymd_and_hms(2020, 1, 1, 9, 0, 0)
                    .single()
                    .expect("unambiguous"),
            },
            next_run_at: Some(Utc::now() - ChronoDuration::seconds(30)),
        })
        .await
        .expect("create task");

    let router = Arc::new(RecordingRouter::default());
    let scheduler = ScheduledTaskScheduler::spawn_with_cadence(
        store.clone(),
        queue.clone(),
        threads.clone(),
        colleagues.clone(),
        router.clone(),
        clock.clone(),
        Duration::from_millis(50),
        None,
    );

    // Poll for the ensure_delivery call. Snapshot out of the guard each tick so
    // no `MutexGuard` is held across the `await`.
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        let snapshot = router.calls.lock().expect("mutex").clone();
        if !snapshot.is_empty() || std::time::Instant::now() >= deadline {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    scheduler.shutdown().await;

    let calls = router.calls.lock().expect("mutex").clone();
    assert_eq!(
        calls.len(),
        1,
        "the fire ensures delivery exactly once for the fired thread"
    );
    assert_eq!(calls[0].0, seed.org_id, "delivery scoped to the task's org");

    // The recorded thread is the one the fire created in `#general`.
    let (thread_id,): (uuid::Uuid,) =
        sqlx::query_as("SELECT id FROM threads WHERE channel_id = $1")
            .bind(general)
            .fetch_one(&pool)
            .await
            .expect("fired thread");
    assert_eq!(
        calls[0].1,
        ThreadId::from(thread_id),
        "ensure_delivery targets the thread the fire created"
    );
}
