//! Integration tests for the turn-metrics recorder. Two layers:
//!
//!   1. [`PgTurnMetricsStore::record`] direct (org-trigger, RLS).
//!   2. The agent loop, with a `ScriptedProvider`, writes one row per
//!      provider call — proving the recorder is wired into `run_turn`
//!      end-to-end (the gap that left the Logs & Metrics tab empty even
//!      after real turns landed).
//!
//! The agent-loop test is the load-bearing one for the regression: if
//! someone forgets the `.with_turn_metrics(...)` call on the builder the
//! second test would go red while the first stayed green.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use chrono::Utc;
use tokio_util::sync::CancellationToken;

use patom::agent_core::AgentBuilder;
use patom::agent_core::turn_metrics::{
    DurationMs, InputTokens, OutputTokens, PgTurnMetricsStore, SharedTurnMetricsStore,
    StopReasonLabel, TurnMetricsId, TurnMetricsRow, TurnMetricsStore,
};
use patom::agents::prompt_versions::PromptVersionId;
use patom::clock::SystemClock;
use patom::hook::HookChain;
use patom::memory::{SharedMemory, StaticMemory};
use patom::provider::{
    AssistantContent, ChatRequest, ChatResponse, LlmProvider, Model, ProviderError,
    ProviderRegistry, SharedProvider, SharedProviderRegistry, StopReason, ToolCall, ToolCallId,
    Usage,
};
use patom::runtime::{PromptRequestId, RequestKindPayload};
use patom::session::{PgSessionStore, SharedSessionStore};
use patom::tools::ToolRegistry;
use patom::types::{Participant, Prompt, ToolName};

mod common;
use common::pg::{Seed, human_to_agent_session, seed_prompt_request, seed_tenant};
use sqlx::PgPool;

/// Pluck the current prompt-version row id for `agent_id`. Seeded by
/// migration 43 to v=1 for every agent — including the default one
/// [`seed_tenant`] creates — so this is total under the harness.
async fn current_prompt_version(
    pool: &sqlx::PgPool,
    agent_id: patom::agents::AgentId,
) -> PromptVersionId {
    sqlx::query_scalar::<_, PromptVersionId>(
        "SELECT id FROM agent_prompt_versions \
         WHERE agent_id = $1 ORDER BY version DESC LIMIT 1",
    )
    .bind(agent_id)
    .fetch_one(pool)
    .await
    .expect("seeded prompt version row")
}

fn fresh_row(
    org_id: patom::auth::OrgId,
    session_id: patom::session::SessionId,
    request_id: PromptRequestId,
    agent_id: patom::agents::AgentId,
    prompt_version_id: PromptVersionId,
) -> TurnMetricsRow {
    TurnMetricsRow {
        id: TurnMetricsId::new(),
        request_id,
        org_id,
        session_id,
        agent_id,
        prompt_version_id,
        kind: patom::runtime::RequestKind::Normal,
        model: Model::try_from("test-model").expect("catalog"),
        provider: patom::provider::ProviderId::Anthropic,
        input_tokens: InputTokens::try_from(10u32).expect("fits"),
        output_tokens: OutputTokens::try_from(20u32).expect("fits"),
        cache_creation_tokens: None,
        cache_read_tokens: None,
        duration_ms: DurationMs::saturating_from_millis(42),
        stop_reason: StopReasonLabel::from_truncated("end_turn"),
        started_at: Utc::now(),
    }
}

#[sqlx::test]
async fn pg_store_records_a_row(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let sessions: SharedSessionStore =
        Arc::new(PgSessionStore::new(pool.clone(), SystemClock::shared()));
    let session =
        human_to_agent_session(sessions.as_ref(), seed.agent_id, seed.org_id, seed.user_id).await;
    let request_id = seed_prompt_request(&pool, session, seed.agent_id, seed.org_id).await;
    let pvid = current_prompt_version(&pool, seed.agent_id).await;

    let store = PgTurnMetricsStore::new(pool.clone(), SystemClock::shared());
    let row = fresh_row(seed.org_id, session, request_id, seed.agent_id, pvid);
    store.record(row).await.expect("record");

    let stored: (i32, i32, String, String) =
        sqlx::query_as("SELECT input_tokens, output_tokens, kind, stop_reason FROM turn_metrics")
            .fetch_one(&pool)
            .await
            .expect("read back");
    assert_eq!(stored.0, 10);
    assert_eq!(stored.1, 20);
    assert_eq!(stored.2, "normal");
    assert_eq!(stored.3, "end_turn");
}

#[sqlx::test]
async fn pg_store_trigger_rejects_org_mismatch(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let sessions: SharedSessionStore =
        Arc::new(PgSessionStore::new(pool.clone(), SystemClock::shared()));
    let session =
        human_to_agent_session(sessions.as_ref(), seed.agent_id, seed.org_id, seed.user_id).await;
    let request_id = seed_prompt_request(&pool, session, seed.agent_id, seed.org_id).await;
    let pvid = current_prompt_version(&pool, seed.agent_id).await;

    let store = PgTurnMetricsStore::new(pool.clone(), SystemClock::shared());
    let foreign = patom::auth::OrgId::new();
    let row = fresh_row(foreign, session, request_id, seed.agent_id, pvid);
    let err = store.record(row).await.expect_err("trigger rejects");
    let msg = format!("{err}");
    assert!(
        msg.contains("does not match parent session"),
        "unexpected error: {msg}",
    );
}

// ─── agent-loop wiring ────────────────────────────────────────────────

#[derive(Debug)]
struct ScriptedProvider {
    script: Vec<ChatResponse>,
    cursor: AtomicUsize,
}

impl ScriptedProvider {
    fn new(script: Vec<ChatResponse>) -> Self {
        Self {
            script,
            cursor: AtomicUsize::new(0),
        }
    }
}

#[async_trait]
impl LlmProvider for ScriptedProvider {
    fn name(&self) -> &'static str {
        "scripted"
    }
    async fn send(&self, _req: ChatRequest) -> Result<ChatResponse, ProviderError> {
        let i = self.cursor.fetch_add(1, Ordering::SeqCst);
        self.script
            .get(i)
            .cloned()
            .ok_or_else(|| ProviderError::Transport("script exhausted".into()))
    }
}

fn text_response(s: &str, usage: Usage) -> ChatResponse {
    ChatResponse {
        content: vec![AssistantContent::Text(s.into())],
        stop_reason: StopReason::EndTurn,
        usage,
        ..Default::default()
    }
}

/// A turn that requests one tool call. The tool name need not resolve — an
/// unknown tool folds into an `is_error` `ToolResult` and the loop continues
/// to the next turn, which is exactly the multi-turn shape we need to exercise
/// the per-turn recorder without wiring a real tool.
fn tool_call_response(usage: Usage) -> ChatResponse {
    ChatResponse {
        content: vec![AssistantContent::ToolCall(ToolCall {
            id: ToolCallId::try_from("call-1").expect("tool call id"),
            name: ToolName::try_from("noop").expect("tool name"),
            input: serde_json::json!({}),
        })],
        stop_reason: StopReason::ToolUse,
        usage,
        ..Default::default()
    }
}

/// Build a turn-metrics-wired agent, run one `reply` driven by `script`, and
/// return the `request_id` so the caller can assert on the recorded rows.
/// Shared scaffold for the agent-loop tests below — they differ only in the
/// provider script and what they assert.
async fn run_scripted_reply(
    pool: &PgPool,
    seed: &Seed,
    pvid: PromptVersionId,
    script: Vec<ChatResponse>,
) -> PromptRequestId {
    let provider: SharedProvider = Arc::new(ScriptedProvider::new(script));
    let model = Model::try_from("test-model").expect("catalog");
    let clock = SystemClock::shared();
    let sessions: SharedSessionStore = Arc::new(PgSessionStore::new(pool.clone(), clock.clone()));
    let memory: SharedMemory = Arc::new(StaticMemory::new("test prompt"));
    let providers: SharedProviderRegistry = Arc::new(
        ProviderRegistry::builder()
            .insert(model.provider(), provider)
            .build(),
    );

    let store: SharedTurnMetricsStore = Arc::new(PgTurnMetricsStore::new(pool.clone(), clock));
    let agent = AgentBuilder::new(providers, sessions, memory, model)
        .expect("builder")
        .with_builtin_tools(ToolRegistry::builder().build())
        .with_hooks(HookChain::new())
        .with_turn_metrics(store, seed.agent_id, pvid)
        .build();

    let session = human_to_agent_session(
        &PgSessionStore::new(pool.clone(), SystemClock::shared()),
        seed.agent_id,
        seed.org_id,
        seed.user_id,
    )
    .await;
    let request_id = seed_prompt_request(pool, session, seed.agent_id, seed.org_id).await;
    let prompt = Prompt::try_from("hello").expect("prompt");

    agent
        .reply(
            session,
            Participant::agent(seed.agent_id),
            vec![prompt],
            request_id,
            patom::auth::Caller::new(seed.user_id, seed.org_id),
            RequestKindPayload::Normal {},
            CancellationToken::new(),
            None,
        )
        .await
        .expect("reply");
    request_id
}

#[sqlx::test]
async fn agent_loop_records_one_row_per_provider_call(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let pvid = current_prompt_version(&pool, seed.agent_id).await;

    let request_id = run_scripted_reply(
        &pool,
        &seed,
        pvid,
        vec![text_response(
            "hi back",
            Usage {
                input_tokens: 100,
                output_tokens: 7,
                cache_creation_input_tokens: None,
                cache_read_input_tokens: Some(50),
            },
        )],
    )
    .await;

    let stored: (i32, i32, Option<i32>, String) = sqlx::query_as(
        "SELECT input_tokens, output_tokens, cache_read_tokens, kind FROM turn_metrics WHERE request_id = $1",
    )
    .bind(request_id)
    .fetch_one(&pool)
    .await
    .expect("one row recorded for the turn");
    assert_eq!(stored.0, 100);
    assert_eq!(stored.1, 7);
    assert_eq!(stored.2, Some(50));
    assert_eq!(stored.3, "normal");
}

/// Regression for the `turn_metrics_pkey` 23505 collision: a reply that runs
/// more than one provider call (turn 0 issues a tool call, turn 1 answers)
/// must record one row *per turn*. `request_id` is constant across the loop,
/// so keying the table on it dropped every turn after the first.
#[sqlx::test]
async fn agent_loop_records_one_row_per_turn_in_multi_turn_reply(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let pvid = current_prompt_version(&pool, seed.agent_id).await;

    let request_id = run_scripted_reply(
        &pool,
        &seed,
        pvid,
        vec![
            // Turn 0: ask for a tool, so the loop continues to a second turn.
            tool_call_response(Usage {
                input_tokens: 100,
                output_tokens: 7,
                cache_creation_input_tokens: None,
                cache_read_input_tokens: None,
            }),
            // Turn 1: final answer.
            text_response(
                "done",
                Usage {
                    input_tokens: 120,
                    output_tokens: 9,
                    cache_creation_input_tokens: None,
                    cache_read_input_tokens: None,
                },
            ),
        ],
    )
    .await;

    let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM turn_metrics WHERE request_id = $1")
        .bind(request_id)
        .fetch_one(&pool)
        .await
        .expect("count rows");
    assert_eq!(rows, 2, "expected one turn_metrics row per provider call");
}
