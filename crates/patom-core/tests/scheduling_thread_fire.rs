//! P9: a scheduled task is the third trigger source.
//!
//! When a due task fires, the scheduler *initiates a thread* in the task's
//! target channel, seeds the task prompt as an owner-private instruction, and
//! wakes the owning agent with a `Normal` chat trigger. The agent (read-at-run)
//! replies through `send_message`, posting a summary tagging the task owner —
//! whose channel membership the human gate confirms.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use chrono::{Duration as ChronoDuration, TimeZone, Utc};

use patom::agent_core::AgentBuilder;
use patom::agents::{
    AGENT_PROMPT_CACHE_CAP, AGENT_PROMPT_CACHE_TTL, AgentFactory, CachedAgents, SharedAgentStore,
    SharedAgents,
};
use patom::clock::SystemClock;
use patom::colleagues::{PgColleagueStore, SharedColleagueStore, resolve_user_colleague};
use patom::hook::HookChain;
use patom::memory::{SharedMemory, StaticMemory};
use patom::provider::{
    AssistantContent, ChatRequest, ChatResponse, LlmProvider, Model, ProviderError, ProviderId,
    ProviderRegistry, SharedProvider, SharedProviderRegistry, StopReason, ToolCall, ToolCallId,
};
use patom::runtime::{
    PgDagBudget, PgPromptQueue, PgResponseHub, SharedDagBudget, SharedPromptQueue,
    SharedResponseSink, WorkerConfig, WorkerPool,
};
use patom::scheduling::{
    NewScheduledTask, PgScheduledTaskStore, ScheduleSpec, ScheduledPrompt, ScheduledTaskName,
    ScheduledTaskScheduler, ScheduledTaskState, SharedScheduledTaskStore,
};
use patom::threads::{PgThreadStore, SharedThreadStore};
use patom::tools::system::SendMessageTool;
use patom::tools::{ToolBox, ToolRegistry};
use patom::types::ToolName;
use sqlx::PgPool;

mod common;
use common::pg::seed_tenant;

/// Provider that posts a summary via `send_message(human, …)` on its first
/// call, then ends the turn. Stands in for "the agent" — the scheduled fire's
/// plumbing is what the test exercises, not the model's reasoning.
#[derive(Debug, Default)]
struct PostsSummary {
    cursor: AtomicUsize,
}

const SUMMARY: &str = "Your morning summary: all clear.";

#[async_trait]
impl LlmProvider for PostsSummary {
    fn name(&self) -> &'static str {
        "posts-summary"
    }

    async fn send(&self, _request: ChatRequest) -> Result<ChatResponse, ProviderError> {
        let i = self.cursor.fetch_add(1, Ordering::SeqCst);
        if i == 0 {
            Ok(ChatResponse {
                content: vec![AssistantContent::ToolCall(ToolCall {
                    id: ToolCallId::try_from("call-1").expect("id"),
                    name: ToolName::try_from("send_message").expect("name"),
                    input: serde_json::json!({
                        "receiver": { "kind": "human" },
                        "content": SUMMARY,
                    }),
                })],
                stop_reason: StopReason::ToolUse,
                ..Default::default()
            })
        } else {
            Ok(ChatResponse {
                content: vec![AssistantContent::Text("done".into())],
                stop_reason: StopReason::EndTurn,
                ..Default::default()
            })
        }
    }
}

#[sqlx::test]
async fn fire_creates_thread_and_agent_posts_summary_tagging_owner(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let clock = SystemClock::shared();
    let owner_colleague = resolve_user_colleague(&pool, seed.org_id, seed.user_id)
        .await
        .expect("owner colleague");

    // `#general` is auto-created with the owner enrolled (org_members trigger),
    // so the agent's later `send_message` to the owner clears the human gate.
    let (general,): (uuid::Uuid,) =
        sqlx::query_as("SELECT id FROM channels WHERE org_id = $1 AND name = 'general'")
            .bind(seed.org_id)
            .fetch_one(&pool)
            .await
            .expect("general channel");
    let general = patom::channels::ChannelId::from(general);

    // Collaborators.
    let threads: SharedThreadStore = Arc::new(PgThreadStore::new(pool.clone(), clock.clone()));
    let queue: SharedPromptQueue = Arc::new(PgPromptQueue::new(pool.clone(), clock.clone()));
    let dag: SharedDagBudget = Arc::new(PgDagBudget::new(pool.clone()));
    let hub = Arc::new(PgResponseHub::new(pool.clone(), clock.clone()));
    let sink: SharedResponseSink = hub.clone();
    let colleagues: SharedColleagueStore = Arc::new(PgColleagueStore::new(pool.clone()));
    let agent_store: SharedAgentStore = common::pg::shared_agent_store(pool.clone(), clock.clone());

    // Agent factory: PostsSummary provider + a real send_message tool wired so
    // the agent's reply lands a posted feed row (the egress).
    let provider: SharedProvider = Arc::new(PostsSummary::default());
    let providers: SharedProviderRegistry = Arc::new(
        ProviderRegistry::builder()
            .insert(ProviderId::Anthropic, provider)
            .build(),
    );
    let memory: SharedMemory = Arc::new(StaticMemory::new("test"));
    let model = Model::try_from("test-model").expect("catalog");
    let registry = ToolRegistry::builder()
        .with(Arc::new(SendMessageTool::new(
            threads.clone(),
            queue.clone(),
            dag.clone(),
            agent_store.clone(),
            colleagues.clone(),
            sink.clone(),
        )))
        .build();
    let threads_for_factory = threads.clone();
    let clock_for_factory = clock.clone();
    let factory: AgentFactory = Arc::new(move |_record| {
        AgentBuilder::new(providers.clone(), memory.clone(), model)
            .expect("builder")
            .with_clock(clock_for_factory.clone())
            .with_thread_store(threads_for_factory.clone())
            .with_tools(ToolBox::from_builtins(registry.clone()))
            .with_hooks(HookChain::new())
            .build()
    });
    let agents: SharedAgents = Arc::new(CachedAgents::new(
        agent_store.clone(),
        factory,
        AGENT_PROMPT_CACHE_CAP,
        AGENT_PROMPT_CACHE_TTL,
        clock.clone(),
    ));

    let cfg = WorkerConfig {
        workers: 1,
        max_turn_duration: Duration::from_secs(10),
        idle_poll: Duration::from_millis(20),
        cancel_poll: Duration::from_millis(50),
        ..WorkerConfig::default()
    };
    let workers = WorkerPool::new(queue.clone(), sink, agents, threads.clone(), dag, cfg).spawn();

    // A `Once` task due 30s ago, targeting `#general`, owned by the seeded
    // agent + human. `next_after` returns None for a past `run_at`, so the
    // task transitions to Done after one fire.
    let store: SharedScheduledTaskStore =
        Arc::new(PgScheduledTaskStore::new(pool.clone(), clock.clone()));
    let task_id = store
        .create(NewScheduledTask {
            owner_agent_id: seed.agent_id,
            org_id: seed.org_id,
            created_by_user_id: seed.user_id,
            channel_id: Some(general),
            name: ScheduledTaskName::try_from("morning brief").expect("name"),
            prompt: ScheduledPrompt::try_from("Post the morning summary, tagging me.")
                .expect("prompt"),
            schedule: ScheduleSpec::Once {
                run_at: Utc
                    .with_ymd_and_hms(2020, 1, 1, 9, 0, 0)
                    .single()
                    .expect("unambiguous"),
            },
            next_run_at: Some(Utc::now() - ChronoDuration::seconds(30)),
        })
        .await
        .expect("create task")
        .id;

    let scheduler = ScheduledTaskScheduler::spawn_with_cadence(
        store.clone(),
        queue.clone(),
        threads.clone(),
        colleagues.clone(),
        clock.clone(),
        Duration::from_millis(50),
        None,
    );

    // Poll for the agent's posted reply, addressed to the task owner, in a
    // thread the fire created under `#general`.
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    let mut hit: Option<(uuid::Uuid,)> = None;
    while std::time::Instant::now() < deadline {
        let row: Option<(uuid::Uuid,)> = sqlx::query_as(
            "SELECT m.receiver_colleague_id \
             FROM thread_messages m \
             JOIN threads t ON t.id = m.thread_id \
             WHERE t.channel_id = $1 AND m.kind = 'posted' \
               AND m.receiver_colleague_id = $2 \
             LIMIT 1",
        )
        .bind(general)
        .bind(owner_colleague)
        .fetch_optional(&pool)
        .await
        .expect("poll posted");
        if row.is_some() {
            hit = row;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    scheduler.shutdown().await;
    workers.shutdown().await;

    let (receiver,) = hit.expect("agent posted a summary tagging the owner");
    assert_eq!(
        receiver,
        owner_colleague.as_uuid(),
        "the posted summary is addressed to the task owner",
    );

    // The fire created exactly one thread, in the task's channel.
    let (thread_count,): (i64,) =
        sqlx::query_as("SELECT count(*) FROM threads WHERE channel_id = $1")
            .bind(general)
            .fetch_one(&pool)
            .await
            .expect("count threads");
    assert_eq!(
        thread_count, 1,
        "the fire initiated one thread in the channel"
    );

    // The seeded instruction landed as an owner-private system_note (off the
    // posted timeline), so the channel timeline shows only the agent's reply.
    let (system_notes,): (i64,) = sqlx::query_as(
        "SELECT count(*) FROM thread_messages m JOIN threads t ON t.id = m.thread_id \
         WHERE t.channel_id = $1 AND m.kind = 'system_note' AND m.owner_agent_id = $2",
    )
    .bind(general)
    .bind(seed.agent_id)
    .fetch_one(&pool)
    .await
    .expect("count system notes");
    assert_eq!(
        system_notes, 1,
        "the task prompt is seeded as one owner-private note"
    );

    // Once schedule with no future fire ⇒ task transitions to Done.
    let (state,): (ScheduledTaskState,) =
        sqlx::query_as("SELECT state FROM scheduled_tasks WHERE id = $1")
            .bind(task_id)
            .fetch_one(&pool)
            .await
            .expect("read state");
    assert_eq!(state, ScheduledTaskState::Done);
}
