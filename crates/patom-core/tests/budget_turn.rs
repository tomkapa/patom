//! End-to-end wiring of the per-turn spend budget into the agent loop.
//!
//! Two contracts, mirroring `tests/turn_metrics_record.rs`:
//!   1. A completed turn settles its actual cost into `org_budget_usage`
//!      (proves `.with_budget(...)` + `budget_settle` are wired into `run_turn`).
//!   2. An org already over its cap is blocked *before* the provider call —
//!      `reply` returns `AgentError::BudgetExceeded` and the model is untouched.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use patom::agent_core::{AgentBuilder, AgentError};
use patom::budget::{PgBudgetService, SharedBudgetService};
use patom::clock::SystemClock;
use patom::hook::HookChain;
use patom::memory::{SharedMemory, StaticMemory};
use patom::provider::{
    AssistantContent, ChatRequest, ChatResponse, LlmProvider, Model, ProviderError,
    ProviderRegistry, SharedProvider, SharedProviderRegistry, StopReason, Usage,
};
use patom::runtime::RequestKindPayload;
use patom::session::{PgSessionStore, SharedSessionStore};
use patom::tools::ToolRegistry;
use patom::types::Prompt;
use sqlx::PgPool;

mod common;
use common::pg::{
    human_to_agent_session, seed_period_usage, seed_prompt_request, seed_tenant, set_budget,
};

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
    fn calls(&self) -> usize {
        self.cursor.load(Ordering::SeqCst)
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

/// Build an agent over `provider` with the budget service wired in.
fn budgeted_agent(
    pool: &PgPool,
    provider: SharedProvider,
) -> (patom::agent_core::Agent, SharedSessionStore) {
    let model = Model::try_from("test-model").expect("catalog");
    let clock = SystemClock::shared();
    let sessions: SharedSessionStore = Arc::new(PgSessionStore::new(pool.clone(), clock.clone()));
    let memory: SharedMemory = Arc::new(StaticMemory::new("test prompt"));
    let providers: SharedProviderRegistry = Arc::new(
        ProviderRegistry::builder()
            .insert(model.provider(), provider)
            .build(),
    );
    let budget: SharedBudgetService = Arc::new(PgBudgetService::new(pool.clone(), clock.clone()));
    let agent = AgentBuilder::new(providers, sessions.clone(), memory, model)
        .expect("builder")
        .with_builtin_tools(ToolRegistry::builder().build())
        .with_hooks(HookChain::new())
        .with_budget(budget)
        .build();
    (agent, sessions)
}

#[sqlx::test]
async fn completed_turn_settles_its_cost(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    // test-model is priced like Sonnet: input $3/Mtok, output $15/Mtok,
    // cache_read $0.30/Mtok. 100 input + 7 output + 50 cache_read =
    // 300 + 105 + 15 = 420 micro-USD.
    let provider = Arc::new(ScriptedProvider::new(vec![text_response(
        "hi back",
        Usage {
            input_tokens: 100,
            output_tokens: 7,
            cache_creation_input_tokens: None,
            cache_read_input_tokens: Some(50),
        },
    )]));
    let (agent, sessions) = budgeted_agent(&pool, provider);

    let session =
        human_to_agent_session(&pool, sessions.as_ref(), seed.agent_id, seed.org_id, seed.user_id).await;
    let request_id = seed_prompt_request(&pool, session, seed.agent_id, seed.org_id).await;

    agent
        .reply(
            session,
            common::pg::agent_participant(&pool, seed.org_id, seed.agent_id).await,
            vec![Prompt::try_from("hello").expect("prompt")],
            request_id,
            patom::auth::Caller::new(seed.user_id, seed.org_id),
            RequestKindPayload::Normal {},
            CancellationToken::new(),
            None,
        )
        .await
        .expect("reply");

    let (used,): (i64,) =
        sqlx::query_as("SELECT used_micro_usd FROM org_budget_usage WHERE org_id = $1")
            .bind(seed.org_id)
            .fetch_one(&pool)
            .await
            .expect("usage row settled");
    assert_eq!(used, 420);
}

#[sqlx::test]
async fn over_cap_org_blocks_the_turn_before_the_provider(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    // Cap at 1 micro-USD with 1000 already spent this period.
    set_budget(&pool, seed.org_id, Some(1), 8000).await;
    seed_period_usage(&pool, seed.org_id, 1000).await;

    let provider = Arc::new(ScriptedProvider::new(vec![text_response(
        "should never run",
        Usage::default(),
    )]));
    let provider_probe = provider.clone();
    let (agent, sessions) = budgeted_agent(&pool, provider);

    let session =
        human_to_agent_session(&pool, sessions.as_ref(), seed.agent_id, seed.org_id, seed.user_id).await;
    let request_id = seed_prompt_request(&pool, session, seed.agent_id, seed.org_id).await;

    let err = agent
        .reply(
            session,
            common::pg::agent_participant(&pool, seed.org_id, seed.agent_id).await,
            vec![Prompt::try_from("hello").expect("prompt")],
            request_id,
            patom::auth::Caller::new(seed.user_id, seed.org_id),
            RequestKindPayload::Normal {},
            CancellationToken::new(),
            None,
        )
        .await
        .expect_err("over-cap org must be blocked");
    assert!(
        matches!(err, AgentError::BudgetExceeded { org } if org == seed.org_id),
        "expected BudgetExceeded, got {err:?}",
    );
    assert_eq!(
        provider_probe.calls(),
        0,
        "gate runs before the provider call"
    );

    // Usage is unchanged — settle never ran (the gate fails first).
    let (used,): (i64,) =
        sqlx::query_as("SELECT used_micro_usd FROM org_budget_usage WHERE org_id = $1")
            .bind(seed.org_id)
            .fetch_one(&pool)
            .await
            .expect("usage row");
    assert_eq!(used, 1000, "blocked turn must not settle any cost");
}
