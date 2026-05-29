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

use patom_rs::agent_core::AgentBuilder;
use patom_rs::agent_core::turn_metrics::{
    DurationMs, InputTokens, OutputTokens, PgTurnMetricsStore, SharedTurnMetricsStore,
    StopReasonLabel, TurnMetricsRow, TurnMetricsStore,
};
use patom_rs::agents::prompt_versions::PromptVersionId;
use patom_rs::clock::SystemClock;
use patom_rs::hook::HookChain;
use patom_rs::memory::{SharedMemory, StaticMemory};
use patom_rs::provider::{
    AssistantContent, ChatRequest, ChatResponse, LlmProvider, Model, ProviderError,
    ProviderRegistry, SharedProvider, SharedProviderRegistry, StopReason, Usage,
};
use patom_rs::runtime::{PromptRequestId, RequestKindPayload};
use patom_rs::session::{PgSessionStore, SharedSessionStore};
use patom_rs::tools::ToolRegistry;
use patom_rs::types::{Participant, Prompt};

mod common;
use common::pg::{human_to_agent_session, seed_prompt_request, seed_tenant};
use sqlx::PgPool;

/// Pluck the current prompt-version row id for `agent_id`. Seeded by
/// migration 43 to v=1 for every agent — including the default one
/// [`seed_tenant`] creates — so this is total under the harness.
async fn current_prompt_version(
    pool: &sqlx::PgPool,
    agent_id: patom_rs::agents::AgentId,
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
    org_id: patom_rs::auth::OrgId,
    session_id: patom_rs::session::SessionId,
    request_id: PromptRequestId,
    agent_id: patom_rs::agents::AgentId,
    prompt_version_id: PromptVersionId,
) -> TurnMetricsRow {
    TurnMetricsRow {
        request_id,
        org_id,
        session_id,
        agent_id,
        prompt_version_id,
        kind: patom_rs::runtime::RequestKind::Normal,
        model: Model::try_from("test-model").expect("catalog"),
        provider: patom_rs::provider::ProviderId::Anthropic,
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
    let foreign = patom_rs::auth::OrgId::new();
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

#[sqlx::test]
async fn agent_loop_records_one_row_per_provider_call(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let pvid = current_prompt_version(&pool, seed.agent_id).await;

    let provider = Arc::new(ScriptedProvider::new(vec![text_response(
        "hi back",
        Usage {
            input_tokens: 100,
            output_tokens: 7,
            cache_creation_input_tokens: None,
            cache_read_input_tokens: Some(50),
        },
    )]));
    let provider: SharedProvider = provider;
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
    let request_id = seed_prompt_request(&pool, session, seed.agent_id, seed.org_id).await;
    let prompt = Prompt::try_from("hello").expect("prompt");

    agent
        .reply(
            session,
            Participant::agent(seed.agent_id),
            vec![prompt],
            request_id,
            patom_rs::auth::Caller::new(seed.user_id, seed.org_id),
            RequestKindPayload::Normal {},
            CancellationToken::new(),
            None,
        )
        .await
        .expect("reply");

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
