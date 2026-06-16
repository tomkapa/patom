//! Stage 8 + 10 (#182): the inline compaction path, end-to-end through
//! `reply_in_thread`. Proves a long thread triggers a summarizer fold, the
//! summary is persisted + folded into the next prompt, the fold is metered to
//! the org, and a summarizer failure degrades safely to the windowing floor
//! with a cooldown.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use patom::agent_core::AgentBuilder;
use patom::agents::PromptVersionId;
use patom::auth::Caller;
use patom::clock::SystemClock;
use patom::colleagues::{ColleagueId, resolve_agent_colleague, resolve_user_colleague};
use patom::provider::{
    AssistantContent, ChatMessage, ChatRequest, ChatResponse, LlmProvider, Model, ProviderError,
    ProviderRegistry, SharedProvider, SharedProviderRegistry, StopReason, UserContent,
};
use patom::runtime::{IdempotencyKey, NewTrigger, PgPromptQueue, PromptQueue, RequestKindPayload};
use patom::threads::{
    AgentThreadId, MessageKind, NewMessage, PgThreadStore, SharedThreadStore, ThreadId,
};
use patom::tools::ToolRegistry;
use sqlx::PgPool;
use uuid::Uuid;

mod common;
use common::pg::{agent_participant, seed_tenant};

/// A summarizer fold is the request whose system prompt is the compactor's —
/// it contains this phrase, while a normal turn's system prompt does not.
const FOLD_MARKER: &str = "rolling summary";

fn is_fold(req: &ChatRequest) -> bool {
    req.system.contains(FOLD_MARKER)
}

fn text_response(s: &str) -> ChatResponse {
    ChatResponse {
        content: vec![AssistantContent::Text(s.into())],
        stop_reason: StopReason::EndTurn,
        ..Default::default()
    }
}

/// Replays `fold_reply` for any summarizer fold and `turn_reply` for the actual
/// turn, recording every request. Lets a test inspect the fold and the turn
/// independently regardless of ordering.
#[derive(Debug)]
struct SplitProvider {
    fold_reply: String,
    turn_reply: String,
    fold_fails: bool,
    fold_calls: AtomicUsize,
    seen: std::sync::Mutex<Vec<ChatRequest>>,
}

impl SplitProvider {
    fn new(fold_reply: &str, turn_reply: &str, fold_fails: bool) -> Self {
        Self {
            fold_reply: fold_reply.to_string(),
            turn_reply: turn_reply.to_string(),
            fold_fails,
            fold_calls: AtomicUsize::new(0),
            seen: std::sync::Mutex::new(Vec::new()),
        }
    }
    fn fold_calls(&self) -> usize {
        self.fold_calls.load(Ordering::SeqCst)
    }
    fn turn_requests(&self) -> Vec<ChatRequest> {
        self.seen
            .lock()
            .unwrap()
            .iter()
            .filter(|r| !is_fold(r))
            .cloned()
            .collect()
    }
}

#[async_trait]
impl LlmProvider for SplitProvider {
    fn name(&self) -> &'static str {
        "split"
    }
    async fn send(&self, request: ChatRequest) -> Result<ChatResponse, ProviderError> {
        let fold = is_fold(&request);
        self.seen.lock().unwrap().push(request);
        if fold {
            self.fold_calls.fetch_add(1, Ordering::SeqCst);
            if self.fold_fails {
                return Err(ProviderError::Transport("scripted fold failure".into()));
            }
            return Ok(text_response(&self.fold_reply));
        }
        Ok(text_response(&self.turn_reply))
    }
}

struct Harness {
    threads: SharedThreadStore,
    queue: PgPromptQueue,
    caller: Caller,
    human: ColleagueId,
    agent_col: ColleagueId,
    thread: ThreadId,
    state: AgentThreadId,
    org_id: patom::auth::OrgId,
    user_id: patom::auth::UserId,
    agent_id: patom::agents::AgentId,
}

async fn setup(pool: &PgPool, posts: usize, chars_each: usize) -> Harness {
    let seed = seed_tenant(pool).await;
    let clock = SystemClock::shared();
    let threads: SharedThreadStore = Arc::new(PgThreadStore::new(pool.clone(), clock.clone()));
    let queue = PgPromptQueue::new(pool.clone(), clock);
    let caller = Caller::new(seed.user_id, seed.org_id);
    let human = resolve_user_colleague(pool, seed.org_id, seed.user_id)
        .await
        .expect("human");
    let agent_col = resolve_agent_colleague(pool, seed.org_id, seed.agent_id)
        .await
        .expect("agent colleague");
    let thread = threads
        .create_thread(&caller, None, None, human, Some(agent_col))
        .await
        .expect("thread");
    let state = threads
        .resolve_participation(&caller, thread, seed.agent_id)
        .await
        .expect("participation");
    seed_posts(
        &threads, &caller, thread, human, agent_col, posts, chars_each, 0,
    )
    .await;
    Harness {
        threads,
        queue,
        caller,
        human,
        agent_col,
        thread,
        state,
        org_id: seed.org_id,
        user_id: seed.user_id,
        agent_id: seed.agent_id,
    }
}

#[allow(clippy::too_many_arguments)]
async fn seed_posts(
    threads: &SharedThreadStore,
    caller: &Caller,
    thread: ThreadId,
    human: ColleagueId,
    agent_col: ColleagueId,
    count: usize,
    chars_each: usize,
    start: usize,
) {
    for i in start..start + count {
        threads
            .append(
                caller,
                thread,
                NewMessage {
                    kind: MessageKind::Posted,
                    sender: Some(human),
                    owner_agent_id: None,
                    receiver: Some(agent_col),
                    body: ChatMessage::User(vec![UserContent::Text(format!(
                        "msg{i}: {}",
                        "context ".repeat(chars_each / 8)
                    ))]),
                    request_id: None,
                    idempotency_key: None,
                },
            )
            .await
            .expect("seed post");
    }
}

async fn enqueue(h: &Harness) -> patom::runtime::PromptRequestId {
    h.queue
        .enqueue_trigger(NewTrigger {
            org_id: h.org_id,
            acting_user_id: h.user_id,
            thread_id: Some(h.thread),
            state_id: Some(h.state),
            background_turn_id: None,
            sender_colleague_id: h.human,
            receiver_agent_id: h.agent_id,
            root_request_id: None,
            trigger_message_id: None,
            idempotency_key: IdempotencyKey::try_from(format!("tag-{}", Uuid::new_v4()))
                .expect("key"),
            kind_payload: RequestKindPayload::Normal {},
        })
        .await
        .expect("enqueue")
}

async fn prompt_version(pool: &PgPool, agent_id: patom::agents::AgentId) -> PromptVersionId {
    sqlx::query_scalar(
        "SELECT id FROM agent_prompt_versions WHERE agent_id = $1 ORDER BY version DESC LIMIT 1",
    )
    .bind(agent_id)
    .fetch_one(pool)
    .await
    .expect("agent has a prompt version")
}

fn build_agent(
    h: &Harness,
    provider: SharedProvider,
    turn_metrics: Option<(
        patom::agent_core::turn_metrics::SharedTurnMetricsStore,
        PromptVersionId,
    )>,
) -> patom::agent_core::Agent {
    let model = Model::try_from("test-model").expect("test model");
    let providers: SharedProviderRegistry = Arc::new(
        ProviderRegistry::builder()
            .insert(model.provider(), provider)
            .build(),
    );
    let memory: patom::memory::SharedMemory =
        Arc::new(patom::memory::StaticMemory::new("system prompt"));
    let mut builder = AgentBuilder::new(providers, memory, model)
        .expect("builder")
        .with_thread_store(h.threads.clone())
        .with_builtin_tools(ToolRegistry::empty());
    if let Some((store, pv)) = turn_metrics {
        builder = builder.with_turn_metrics(store, h.agent_id, pv);
    }
    builder.build()
}

/// Stage 8: a thread past the token budget triggers an inline fold; the summary
/// is persisted, folded into the turn's system prefix, the verbatim tail is
/// reduced, and the fold is metered as a `kind='compaction'` turn_metrics row.
#[sqlx::test]
async fn overflow_triggers_inline_compaction_and_meters(pool: PgPool) {
    let h = setup(&pool, 20, 320).await;
    let request_id = enqueue(&h).await;
    let pv = prompt_version(&pool, h.agent_id).await;
    let metrics: patom::agent_core::turn_metrics::SharedTurnMetricsStore =
        Arc::new(patom::agent_core::turn_metrics::PgTurnMetricsStore::new(
            pool.clone(),
            SystemClock::shared(),
        ));

    let provider = Arc::new(SplitProvider::new("ROLLING-SUMMARY-A", "done", false));
    let shared: SharedProvider = provider.clone();
    let agent = build_agent(&h, shared, Some((metrics, pv)));
    let viewer = agent_participant(&pool, h.org_id, h.agent_id).await;

    let reply = agent
        .reply_in_thread(
            h.state,
            h.thread,
            viewer,
            request_id,
            request_id,
            h.caller,
            RequestKindPayload::Normal {},
            CancellationToken::new(),
            None,
        )
        .await
        .expect("reply");
    assert_eq!(reply.final_text(), "done");
    assert_eq!(provider.fold_calls(), 1, "one inline fold ran");

    // The compaction is persisted and advances coverage.
    let comp = h
        .threads
        .load_compaction(h.thread, h.agent_id)
        .await
        .expect("load")
        .expect("a compaction row");
    assert!(comp.summary.contains("ROLLING-SUMMARY-A"));
    assert!(comp.covers_through_seq.get() > 0);

    // The turn's prompt carries the summary in the protected head and a reduced tail.
    let turns = provider.turn_requests();
    assert_eq!(turns.len(), 1, "exactly one turn call");
    let turn = &turns[0];
    assert!(
        turn.system.contains("Earlier conversation (compacted)"),
        "summary folded into the system prefix"
    );
    assert!(turn.system.contains("ROLLING-SUMMARY-A"));
    assert!(turn.messages.len() < 20, "verbatim tail is reduced");

    // The fold is metered to the org under a `compaction` turn_metrics row.
    let compaction_metrics: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM turn_metrics WHERE state_id = $1 AND kind = 'compaction'",
    )
    .bind(h.state)
    .fetch_one(&pool)
    .await
    .expect("count");
    assert_eq!(compaction_metrics, 1, "the fold was metered");
}

/// Stage 10: when the summarizer fails, the turn still completes (the windowing
/// floor holds), a cooldown is recorded, and the next overflow turn skips the
/// LLM entirely rather than re-paying the failed call.
#[sqlx::test]
async fn summarizer_failure_falls_back_and_cools_down(pool: PgPool) {
    let h = setup(&pool, 20, 320).await;
    let provider = Arc::new(SplitProvider::new("unused", "done", true));
    let shared: SharedProvider = provider.clone();
    let agent = build_agent(&h, shared, None);
    let viewer = agent_participant(&pool, h.org_id, h.agent_id).await;

    // Turn 1: the fold errors, but the turn completes on the floor.
    let r1 = enqueue(&h).await;
    let reply = agent
        .reply_in_thread(
            h.state,
            h.thread,
            viewer,
            r1,
            r1,
            h.caller,
            RequestKindPayload::Normal {},
            CancellationToken::new(),
            None,
        )
        .await
        .expect("reply 1 still succeeds");
    assert_eq!(reply.final_text(), "done");
    assert_eq!(
        provider.fold_calls(),
        1,
        "the failing fold was attempted once"
    );
    assert_eq!(provider.turn_requests().len(), 1, "the turn still ran");

    let comp = h
        .threads
        .load_compaction(h.thread, h.agent_id)
        .await
        .expect("load")
        .expect("a cooldown row");
    assert!(comp.cooldown_until.is_some(), "a cooldown was set");
    assert!(comp.failed_attempts >= 1);

    // Turn 2: still in cooldown -> the overflow path skips the summarizer.
    seed_posts(
        &h.threads,
        &h.caller,
        h.thread,
        h.human,
        h.agent_col,
        4,
        320,
        100,
    )
    .await;
    let r2 = enqueue(&h).await;
    agent
        .reply_in_thread(
            h.state,
            h.thread,
            viewer,
            r2,
            r2,
            h.caller,
            RequestKindPayload::Normal {},
            CancellationToken::new(),
            None,
        )
        .await
        .expect("reply 2");
    assert_eq!(
        provider.fold_calls(),
        1,
        "cooldown suppressed a second fold attempt"
    );
}
