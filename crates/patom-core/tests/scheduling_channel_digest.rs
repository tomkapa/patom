//! #199: a fired **channel** task seeds a digest-window cursor.
//!
//! The scheduler's seed for a channel task carries a `<digest-window since=…/>`
//! footer so the agent reads only what is new since the previous run. This
//! exercises the real fire path (no worker needed — the seed lands before any
//! agent turn) and asserts the window tag + the base prompt are both present in
//! the owner-private `system_note`. The exact `since` cursor (creation vs prior
//! fire) is pinned by the `seed_prompt_text` unit tests in `scheduler.rs`.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::sync::Arc;
use std::time::Duration;

use chrono::{Duration as ChronoDuration, TimeZone, Utc};
use patom::clock::SystemClock;
use patom::colleagues::{PgColleagueStore, SharedColleagueStore};
use patom::runtime::{PgPromptQueue, SharedPromptQueue};
use patom::scheduling::{
    NewScheduledTask, PgScheduledTaskStore, ScheduleSpec, ScheduledPrompt, ScheduledTaskName,
    ScheduledTaskScheduler, SharedScheduledTaskStore,
};
use patom::threads::{PgThreadStore, SharedThreadStore};
use sqlx::PgPool;

mod common;
use common::pg::seed_tenant;

#[sqlx::test]
async fn channel_task_seed_carries_digest_window(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let clock = SystemClock::shared();

    let (general,): (uuid::Uuid,) =
        sqlx::query_as("SELECT id FROM channels WHERE org_id = $1 AND name = 'general'")
            .bind(seed.org_id)
            .fetch_one(&pool)
            .await
            .expect("general channel");
    let general = patom::channels::ChannelId::from(general);

    let threads: SharedThreadStore = Arc::new(PgThreadStore::new(pool.clone(), clock.clone()));
    let queue: SharedPromptQueue = Arc::new(PgPromptQueue::new(pool.clone(), clock.clone()));
    let colleagues: SharedColleagueStore = Arc::new(PgColleagueStore::new(pool.clone()));
    let store: SharedScheduledTaskStore =
        Arc::new(PgScheduledTaskStore::new(pool.clone(), clock.clone()));

    // A channel task due 30s ago, owned by the seeded agent + human.
    store
        .create(NewScheduledTask {
            owner_agent_id: seed.agent_id,
            org_id: seed.org_id,
            created_by_user_id: seed.user_id,
            channel_id: Some(general),
            name: ScheduledTaskName::try_from("overnight digest").expect("name"),
            prompt: ScheduledPrompt::try_from("Summarise overnight activity.").expect("prompt"),
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

    let scheduler = ScheduledTaskScheduler::spawn_with_cadence(
        store.clone(),
        queue.clone(),
        threads.clone(),
        colleagues.clone(),
        Arc::new(patom::outbound::NoopOutboundRouter),
        clock.clone(),
        Duration::from_millis(50),
        None,
    );

    // Poll for the seeded owner-private system_note in a thread under #general.
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    let mut body: Option<serde_json::Value> = None;
    while std::time::Instant::now() < deadline {
        let row: Option<(serde_json::Value,)> = sqlx::query_as(
            "SELECT m.body FROM thread_messages m JOIN threads t ON t.id = m.thread_id \
             WHERE t.channel_id = $1 AND m.kind = 'system_note' AND m.owner_agent_id = $2 \
             LIMIT 1",
        )
        .bind(general)
        .bind(seed.agent_id)
        .fetch_optional(&pool)
        .await
        .expect("poll seed");
        if let Some((b,)) = row {
            body = Some(b);
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    scheduler.shutdown().await;

    let body = body.expect("the fire seeded a system_note for the channel task");
    let text = body["contents"][0]["value"]
        .as_str()
        .expect("seed note carries text");
    assert!(
        text.contains("Summarise overnight activity."),
        "the seed keeps the task's base prompt, got: {text}"
    );
    assert!(
        text.contains("<digest-window since=\""),
        "a channel task's seed carries the digest-window cursor, got: {text}"
    );
}
