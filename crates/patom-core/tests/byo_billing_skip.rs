//! BYO turns bypass the platform credit gate and debit (#141).
//!
//! The escape valve past the #154 zero-balance gate: an org with no platform
//! credit but its own provider key keeps running, and those turns never touch
//! the credit ledger. The complement — same zero-balance org *without* a key —
//! is blocked with `OutOfCredit`. Driven end-to-end through the real worker so
//! the gate/settle wrappers, the live overlay read, and the billing service all
//! participate.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;

use patom::agent_core::AgentBuilder;
use patom::agents::{
    AGENT_PROMPT_CACHE_CAP, AGENT_PROMPT_CACHE_TTL, AgentFactory, CachedAgents, SharedAgents,
};
use patom::auth::Caller;
use patom::billing::SharedBillingService;
use patom::clock::SystemClock;
use patom::colleagues::{resolve_agent_colleague, resolve_user_colleague};
use patom::hook::HookChain;
use patom::memory::{SharedMemory, StaticMemory};
use patom::provider::{
    AssistantContent, ChatMessage, ChatRequest, ChatResponse, LlmProvider, Model,
    OrgProviderOverlay, ProviderError, ProviderId, ProviderRegistry, SharedProvider,
    SharedProviderRegistry, StopReason, UserContent,
};
use patom::runtime::{
    FailureReason, IdempotencyKey, NewTrigger, PgDagBudget, PgPromptQueue, PgResponseHub,
    RequestKindPayload, SharedDagBudget, SharedPromptQueue, SharedResponseSink, WorkerConfig,
    WorkerPool,
};
use patom::threads::{MessageKind, NewMessage, PgThreadStore, SharedThreadStore};
use patom::tools::{ToolBox, ToolRegistry};
use sqlx::PgPool;
use uuid::Uuid;

mod common;
use common::billing::{active_service, read_org_credits};
use common::pg::{Seed, seed_tenant};

/// Always replies with plain text — never `send_message` — so the turn produces
/// no egress and the ping-pong guard eventually parks it as `NoEgress`. Each
/// turn still runs the billing gate + settle, which is what we assert on.
#[derive(Debug)]
struct AlwaysText;

#[async_trait]
impl LlmProvider for AlwaysText {
    fn name(&self) -> &'static str {
        "always-text"
    }
    async fn send(&self, _request: ChatRequest) -> Result<ChatResponse, ProviderError> {
        Ok(ChatResponse {
            content: vec![AssistantContent::Text("thinking".into())],
            stop_reason: StopReason::EndTurn,
            ..Default::default()
        })
    }
}

/// Build a cached-agent registry whose factory wires the given billing service
/// and BYO overlay (the agent under test). `test-model` resolves to
/// `ProviderId::Anthropic` in the test catalog; the platform registry serves it
/// too so the no-key path has somewhere to route.
fn build_agents(
    pool: &PgPool,
    clock: &patom::clock::SharedClock,
    threads: &SharedThreadStore,
    billing: SharedBillingService,
    overlay: OrgProviderOverlay,
    org_id: patom::auth::OrgId,
) -> SharedAgents {
    let model = Model::try_from("test-model").expect("catalog");
    let platform: SharedProvider = Arc::new(AlwaysText);
    let providers: SharedProviderRegistry = Arc::new(
        ProviderRegistry::builder()
            .insert(ProviderId::Anthropic, platform)
            .build(),
    );
    let memory: SharedMemory = Arc::new(StaticMemory::new("test"));
    let threads = threads.clone();
    let clock_for_factory = clock.clone();
    let factory: AgentFactory = Arc::new(move |_record| {
        AgentBuilder::new(providers.clone(), memory.clone(), model)
            .expect("builder")
            .with_clock(clock_for_factory.clone())
            .with_thread_store(threads.clone())
            .with_tools(ToolBox::from_builtins(ToolRegistry::empty()))
            .with_hooks(HookChain::new())
            .with_billing(billing.clone())
            .with_org_routing(org_id, overlay.clone())
            .build()
    });
    Arc::new(CachedAgents::new(
        common::pg::shared_agent_store(pool.clone(), clock.clone()),
        factory,
        AGENT_PROMPT_CACHE_CAP,
        AGENT_PROMPT_CACHE_TTL,
        clock.clone(),
    ))
}

/// Drive one human-tag trigger to a terminal status with the given billing
/// service + BYO overlay wired into the agent built for `seed`. Returns the
/// terminal failure reason (these turns never post, so the reason is the signal).
async fn run_turn(
    pool: &PgPool,
    seed: &Seed,
    billing: SharedBillingService,
    overlay: OrgProviderOverlay,
) -> Option<FailureReason> {
    let clock = SystemClock::shared();
    let caller = Caller::new(seed.user_id, seed.org_id);
    let human = resolve_user_colleague(pool, seed.org_id, seed.user_id)
        .await
        .expect("human colleague");
    let agent_col = resolve_agent_colleague(pool, seed.org_id, seed.agent_id)
        .await
        .expect("agent colleague");

    let threads: SharedThreadStore = Arc::new(PgThreadStore::new(pool.clone(), clock.clone()));
    let queue: SharedPromptQueue = Arc::new(PgPromptQueue::new(pool.clone(), clock.clone()));
    let dag: SharedDagBudget = Arc::new(PgDagBudget::new(pool.clone()));
    let sink: SharedResponseSink = Arc::new(PgResponseHub::new(pool.clone(), clock.clone()));

    let thread = threads
        .create_thread(&caller, None, None, human, Some(agent_col))
        .await
        .expect("thread");
    threads
        .append(
            &caller,
            thread,
            NewMessage {
                kind: MessageKind::Posted,
                sender: Some(human),
                owner_agent_id: None,
                receiver: Some(agent_col),
                body: ChatMessage::User(vec![UserContent::Text("are you there?".into())]),
                request_id: None,
                idempotency_key: None,
            },
        )
        .await
        .expect("seed human post");
    let state = threads
        .resolve_participation(&caller, thread, seed.agent_id)
        .await
        .expect("participation");

    let agents = build_agents(pool, &clock, &threads, billing, overlay, seed.org_id);

    let cfg = WorkerConfig {
        workers: 1,
        max_turn_duration: Duration::from_secs(10),
        idle_poll: Duration::from_millis(20),
        cancel_poll: Duration::from_millis(50),
        ..WorkerConfig::default()
    };
    let workers = WorkerPool::new(queue.clone(), sink, agents, threads.clone(), dag, cfg).spawn();

    let trigger = queue
        .enqueue_trigger(NewTrigger {
            org_id: seed.org_id,
            acting_user_id: seed.user_id,
            thread_id: Some(thread),
            state_id: Some(state),
            background_turn_id: None,
            sender_colleague_id: human,
            receiver_agent_id: seed.agent_id,
            root_request_id: None,
            trigger_message_id: None,
            idempotency_key: IdempotencyKey::try_from(format!("tag-{}", Uuid::new_v4()))
                .expect("key"),
            kind_payload: RequestKindPayload::Normal {},
        })
        .await
        .expect("enqueue trigger");

    let mut terminal = None;
    for _ in 0..200u32 {
        let view = queue.status(trigger).await.expect("status");
        if view.status.is_terminal() {
            terminal = Some(view);
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    workers.shutdown().await;
    terminal
        .expect("trigger reached a terminal status")
        .failure_reason
}

#[sqlx::test]
async fn byo_turn_at_zero_balance_is_not_gated_and_never_debits(pool: PgPool) {
    // Active credit gate, no credit row → zero balance. The org holds a BYO key
    // for the agent's provider, so the turn must run (no OutOfCredit) and never
    // touch the credit ledger.
    let seed = seed_tenant(&pool).await;
    let billing: SharedBillingService = Arc::new(active_service(&pool));
    let byo: SharedProvider = Arc::new(AlwaysText);
    let overlay =
        OrgProviderOverlay::for_test(vec![(seed.org_id, ProviderId::Anthropic, byo)], vec![]);

    let reason = run_turn(&pool, &seed, billing, overlay).await;

    assert!(
        !matches!(reason, Some(FailureReason::OutOfCredit)),
        "BYO turn must not be blocked by the zero-balance gate, got {reason:?}"
    );
    assert!(
        read_org_credits(&pool, seed.org_id).await.is_none(),
        "BYO turn must never create/debit a credit row"
    );
}

#[sqlx::test]
async fn platform_turn_at_zero_balance_is_blocked(pool: PgPool) {
    // Same zero-balance org, but no BYO key → the platform gate blocks the turn
    // with OutOfCredit, and nothing settles.
    let seed = seed_tenant(&pool).await;
    let billing: SharedBillingService = Arc::new(active_service(&pool));
    let overlay = OrgProviderOverlay::empty();

    let reason = run_turn(&pool, &seed, billing, overlay).await;

    assert!(
        matches!(reason, Some(FailureReason::OutOfCredit)),
        "platform turn at zero balance must be blocked, got {reason:?}"
    );
    assert!(
        read_org_credits(&pool, seed.org_id).await.is_none(),
        "a gated turn settles nothing"
    );
}
