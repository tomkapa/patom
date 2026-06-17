//! P5: `send_message` human gate.
//!
//! An agent cannot message a human who is not a member of the thread's channel,
//! and the rejection must NOT auto-add them. Posting to the feed is the egress,
//! so a rejected delivery also leaves no posted row.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::sync::Arc;

use patom::auth::{Caller, OrgId, UserId};
use patom::channels::ChannelId;
use patom::clock::SystemClock;
use patom::colleagues::{
    ColleagueId, PgColleagueStore, SharedColleagueStore, resolve_agent_colleague,
    resolve_user_colleague,
};
use patom::runtime::{
    PgDagBudget, PgPromptQueue, PgResponseHub, PromptRequestId, RequestKindPayload,
    SharedDagBudget, SharedPromptQueue, SharedResponseSink,
};
use patom::threads::{PgThreadStore, SharedThreadStore};
use patom::tools::system::SendMessageTool;
use patom::tools::{Tool, ToolCallContext};
use serde_json::json;
use sqlx::PgPool;

mod common;
use common::pg::{
    agent_participant, seed_agent, seed_agent_thread_state, seed_prompt_request, seed_tenant,
};

/// Insert a second human into the org. The `org_members` trigger mints their
/// colleague and adds them to `#general` — but NOT to any channel created
/// afterwards, which is exactly the "org member, not channel member" case.
async fn seed_second_human(
    pool: &PgPool,
    org_id: OrgId,
) -> (UserId, patom::colleagues::ColleagueId) {
    let user_id = UserId::new();
    sqlx::query(
        "INSERT INTO users (id, email, display_name, created_at, updated_at) \
         VALUES ($1, $2, 'Outsider', now(), now())",
    )
    .bind(user_id)
    .bind(format!(
        "outsider-{}@example.test",
        user_id.as_uuid().simple()
    ))
    .execute(pool)
    .await
    .expect("insert second user");
    sqlx::query(
        "INSERT INTO org_members (org_id, user_id, role, created_at) VALUES ($1, $2, 'member', now())",
    )
    .bind(org_id)
    .bind(user_id)
    .execute(pool)
    .await
    .expect("insert second membership");
    let colleague = resolve_user_colleague(pool, org_id, user_id)
        .await
        .expect("second human colleague");
    (user_id, colleague)
}

#[sqlx::test]
async fn send_to_human_non_member_rejected_no_autoadd(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let clock = SystemClock::shared();
    let threads: SharedThreadStore = Arc::new(PgThreadStore::new(pool.clone(), clock.clone()));
    let caller = Caller::new(seed.user_id, seed.org_id);
    let owner_colleague = resolve_user_colleague(&pool, seed.org_id, seed.user_id)
        .await
        .expect("owner colleague");

    // A custom channel the owner creates; the second human is NOT a member.
    let channel = ChannelId::new();
    sqlx::query(
        "INSERT INTO channels (id, org_id, name, created_by_user_id, created_at) \
         VALUES ($1, $2, 'private-room', $3, now())",
    )
    .bind(channel)
    .bind(seed.org_id)
    .bind(seed.user_id)
    .execute(&pool)
    .await
    .expect("create channel");

    let (outsider_user, outsider_colleague) = seed_second_human(&pool, seed.org_id).await;

    let thread = threads
        .create_thread(&caller, Some(channel), None, owner_colleague, None)
        .await
        .expect("create channel thread");
    let state = threads
        .resolve_participation(&caller, thread, seed.agent_id)
        .await
        .expect("participation");

    // Collaborators (workers idle — we drive the tool directly).
    let queue: SharedPromptQueue = Arc::new(PgPromptQueue::new(pool.clone(), clock.clone()));
    let dag: SharedDagBudget = Arc::new(PgDagBudget::new(pool.clone()));
    let sink: SharedResponseSink = Arc::new(PgResponseHub::new(pool.clone(), clock.clone()));
    let colleagues: SharedColleagueStore = Arc::new(PgColleagueStore::new(pool.clone()));
    let tool = SendMessageTool::new(
        threads.clone(),
        queue,
        dag,
        colleagues,
        sink,
        Arc::new(patom::outbound::NoopOutboundRouter),
    );

    let ctx = ToolCallContext {
        claim_key: patom::runtime::ClaimKey::from(state.as_uuid()),
        thread_id: Some(thread),
        state_id: Some(state),
        viewer: agent_participant(&pool, seed.org_id, seed.agent_id).await,
        root_request_id: PromptRequestId::new(),
        request_id: PromptRequestId::new(),
        kind_payload: RequestKindPayload::Normal {},
        acting_user_id: seed.user_id,
        org_id: seed.org_id,
    };

    let result = tool
        .execute(
            json!({
                "receiver": outsider_colleague,
                "content": "psst, over here",
            }),
            &ctx,
        )
        .await;

    // 1) Rejected as a model-facing invalid input naming the membership gate.
    let err = result.expect_err("non-member human must be rejected");
    let msg = err.to_string();
    assert!(
        msg.contains("not a member"),
        "rejection should name the channel-membership gate, got: {msg}"
    );

    // 2) No auto-add: the outsider gained no channel_members row.
    let (member_count,): (i64,) = sqlx::query_as(
        "SELECT count(*) FROM channel_members WHERE channel_id = $1 AND user_id = $2",
    )
    .bind(channel)
    .bind(outsider_user)
    .fetch_one(&pool)
    .await
    .expect("count membership");
    assert_eq!(
        member_count, 0,
        "rejected delivery must not auto-add the human"
    );

    // 3) Rejected before the egress: no posted row landed in the thread.
    let (posted_count,): (i64,) = sqlx::query_as(
        "SELECT count(*) FROM thread_messages WHERE thread_id = $1 AND kind = 'posted'",
    )
    .bind(thread)
    .fetch_one(&pool)
    .await
    .expect("count posted");
    assert_eq!(posted_count, 0, "a rejected message must not be posted");
}

/// Decision 2 (user-pinned): an agent inside a human↔agent DM can invoke any
/// other agent. The DM counterpart bounds *human visibility*, never agent
/// participation — the invoked agent joins the same DM thread (its own
/// `agent_thread_state`), the posted egress lands, and a trigger is enqueued.
#[sqlx::test]
async fn send_message_agent_to_agent_inside_dm_triggers_and_posts(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let clock = SystemClock::shared();
    let threads: SharedThreadStore = Arc::new(PgThreadStore::new(pool.clone(), clock.clone()));
    let caller = Caller::new(seed.user_id, seed.org_id);
    let human = resolve_user_colleague(&pool, seed.org_id, seed.user_id)
        .await
        .expect("human colleague");
    let counterpart_col =
        patom::colleagues::resolve_agent_colleague(&pool, seed.org_id, seed.agent_id)
            .await
            .expect("counterpart colleague");

    // The DM: human ↔ seed agent (the counterpart).
    let thread = threads
        .create_thread(&caller, None, None, human, Some(counterpart_col))
        .await
        .expect("create dm");
    let state = threads
        .resolve_participation(&caller, thread, seed.agent_id)
        .await
        .expect("participation");

    // A second agent, NOT part of the DM pair.
    let agents = common::pg::shared_agent_store(pool.clone(), clock.clone());
    let second = agents
        .create(patom::agents::NewAgent {
            org_id: seed.org_id,
            name: patom::agents::AgentName::try_from("specialist").expect("name"),
            system_prompt: patom::agents::AgentSystemPrompt::try_from("p").expect("prompt"),
            description: patom::agents::AgentDescription::try_from("d").expect("desc"),
            allowed_mcp_tools: patom::agents::AllowedMcpTools::empty(),
            model: None,
            avatar_url: None,
            edited_by: None,
        })
        .await
        .expect("create second agent")
        .id;

    let queue: SharedPromptQueue = Arc::new(PgPromptQueue::new(pool.clone(), clock.clone()));
    let dag: SharedDagBudget = Arc::new(PgDagBudget::new(pool.clone()));
    let sink: SharedResponseSink = Arc::new(PgResponseHub::new(pool.clone(), clock.clone()));
    let colleagues: SharedColleagueStore = Arc::new(PgColleagueStore::new(pool.clone()));
    let tool = SendMessageTool::new(
        threads.clone(),
        queue.clone(),
        dag,
        colleagues,
        sink,
        Arc::new(patom::outbound::NoopOutboundRouter),
    );

    // The specialist's colleague id — addressing is now by id only.
    let second_colleague = patom::colleagues::resolve_agent_colleague(&pool, seed.org_id, second)
        .await
        .expect("specialist colleague");

    // A real root trigger so the DAG budget bump has a row to debit.
    let root = enqueue_root(&queue, &threads, &pool, &seed, thread).await;

    let ctx = ToolCallContext {
        claim_key: patom::runtime::ClaimKey::from(state.as_uuid()),
        thread_id: Some(thread),
        state_id: Some(state),
        viewer: agent_participant(&pool, seed.org_id, seed.agent_id).await,
        root_request_id: root,
        request_id: root,
        kind_payload: RequestKindPayload::Normal {},
        acting_user_id: seed.user_id,
        org_id: seed.org_id,
    };

    let out = tool
        .execute(
            json!({
                "receiver": second_colleague,
                "content": "colleague, take over the analysis",
            }),
            &ctx,
        )
        .await
        .expect("agent→agent inside a DM must deliver");
    let parsed: serde_json::Value = serde_json::from_str(&out).expect("json");
    assert_eq!(parsed["delivery"], "queued", "the second agent was woken");

    // The egress posted row landed in the DM feed.
    let (posted,): (i64,) = sqlx::query_as(
        "SELECT count(*) FROM thread_messages WHERE thread_id = $1 AND kind = 'posted'",
    )
    .bind(thread)
    .fetch_one(&pool)
    .await
    .expect("count posted");
    assert!(posted >= 1, "the egress row landed");

    // The invoked agent participates in the SAME DM thread.
    let (joined,): (i64,) = sqlx::query_as(
        "SELECT count(*) FROM agent_thread_state WHERE thread_id = $1 AND agent_id = $2",
    )
    .bind(thread)
    .bind(second)
    .fetch_one(&pool)
    .await
    .expect("count participation");
    assert_eq!(joined, 1, "the invoked agent joined the DM thread");

    // And a trigger addressed to it is pending.
    let (triggers,): (i64,) = sqlx::query_as(
        "SELECT count(*) FROM prompt_requests pr \
         JOIN colleagues rc ON rc.id = pr.receiver_colleague_id \
         WHERE pr.thread_id = $1 AND rc.agent_id = $2 AND pr.status = 'pending'",
    )
    .bind(thread)
    .bind(second)
    .fetch_one(&pool)
    .await
    .expect("count triggers");
    assert_eq!(triggers, 1, "the second agent has a pending wake trigger");
}

/// W7: in a DM, `send_message` to a human reaches only the pair — creator or
/// counterpart. A third human (org member, not in the pair) is rejected with
/// the same no-auto-add error as a channel non-member.
#[sqlx::test]
async fn send_message_dm_human_receiver_outside_pair_rejected(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let clock = SystemClock::shared();
    let threads: SharedThreadStore = Arc::new(PgThreadStore::new(pool.clone(), clock.clone()));
    let caller = Caller::new(seed.user_id, seed.org_id);
    let human = resolve_user_colleague(&pool, seed.org_id, seed.user_id)
        .await
        .expect("human colleague");
    let counterpart_col =
        patom::colleagues::resolve_agent_colleague(&pool, seed.org_id, seed.agent_id)
            .await
            .expect("counterpart colleague");
    let (_, outsider_colleague) = seed_second_human(&pool, seed.org_id).await;

    let thread = threads
        .create_thread(&caller, None, None, human, Some(counterpart_col))
        .await
        .expect("create dm");
    let state = threads
        .resolve_participation(&caller, thread, seed.agent_id)
        .await
        .expect("participation");

    let queue: SharedPromptQueue = Arc::new(PgPromptQueue::new(pool.clone(), clock.clone()));
    let dag: SharedDagBudget = Arc::new(PgDagBudget::new(pool.clone()));
    let sink: SharedResponseSink = Arc::new(PgResponseHub::new(pool.clone(), clock.clone()));
    let colleagues: SharedColleagueStore = Arc::new(PgColleagueStore::new(pool.clone()));
    let tool = SendMessageTool::new(
        threads.clone(),
        queue.clone(),
        dag,
        colleagues,
        sink,
        Arc::new(patom::outbound::NoopOutboundRouter),
    );

    // A real root trigger so the posted egress row's request FK resolves.
    let root = enqueue_root(&queue, &threads, &pool, &seed, thread).await;

    let ctx = ToolCallContext {
        claim_key: patom::runtime::ClaimKey::from(state.as_uuid()),
        thread_id: Some(thread),
        state_id: Some(state),
        viewer: agent_participant(&pool, seed.org_id, seed.agent_id).await,
        root_request_id: root,
        request_id: root,
        kind_payload: RequestKindPayload::Normal {},
        acting_user_id: seed.user_id,
        org_id: seed.org_id,
    };

    // A human outside the DM pair is unreachable…
    let err = tool
        .execute(
            json!({
                "receiver": outsider_colleague,
                "content": "you didn't hear this from me",
            }),
            &ctx,
        )
        .await
        .expect_err("a human outside the DM pair must be rejected");
    assert!(
        err.to_string().contains("not a member"),
        "rejection names the gate, got: {err}"
    );

    // …while the DM's own human still is.
    tool.execute(
        json!({
            "receiver": human,
            "content": "here's the summary you asked for",
        }),
        &ctx,
    )
    .await
    .expect("the DM creator is always reachable");
}

/// Omitting `receiver` posts an untagged message to the thread — the agent
/// talking to the room, addressed at no one (`receiver_colleague_id` NULL).
#[sqlx::test]
async fn send_message_omitted_receiver_posts_untagged(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let (tool, ctx, thread) = channel_thread_tool(&pool, &seed).await;

    let out = tool
        .execute(json!({ "content": "thinking aloud" }), &ctx)
        .await
        .expect("an untagged post is valid");
    let parsed: serde_json::Value = serde_json::from_str(&out).expect("json");
    assert_eq!(
        parsed["delivery"], "posted",
        "an untagged post is not queued"
    );

    let (untagged,): (i64,) = sqlx::query_as(
        "SELECT count(*) FROM thread_messages \
         WHERE thread_id = $1 AND kind = 'posted' AND receiver_colleague_id IS NULL",
    )
    .bind(thread)
    .fetch_one(&pool)
    .await
    .expect("count untagged");
    assert_eq!(untagged, 1, "exactly one untagged posted row landed");
}

/// A `receiver` id that resolves to no colleague is a model-facing invalid
/// input, rejected before the egress (no posted row).
#[sqlx::test]
async fn send_message_unknown_colleague_rejected(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let (tool, ctx, thread) = channel_thread_tool(&pool, &seed).await;

    let err = tool
        .execute(
            json!({ "receiver": uuid::Uuid::new_v4(), "content": "x" }),
            &ctx,
        )
        .await
        .expect_err("an unknown colleague id must be rejected");
    assert!(
        err.to_string().contains("unknown colleague"),
        "rejection names the unknown colleague, got: {err}"
    );

    let (posted,): (i64,) = sqlx::query_as(
        "SELECT count(*) FROM thread_messages WHERE thread_id = $1 AND kind = 'posted'",
    )
    .bind(thread)
    .fetch_one(&pool)
    .await
    .expect("count posted");
    assert_eq!(posted, 0, "a rejected message must not be posted");
}

/// Build a channel thread the seed agent runs in, wired to a real
/// `send_message` tool and a ctx whose `request_id` is a live root trigger so
/// the posted egress row's FK resolves. The shared scaffold for the
/// untagged / unknown-receiver cases.
async fn channel_thread_tool(
    pool: &PgPool,
    seed: &common::pg::Seed,
) -> (SendMessageTool, ToolCallContext, patom::threads::ThreadId) {
    let clock = SystemClock::shared();
    let threads: SharedThreadStore = Arc::new(PgThreadStore::new(pool.clone(), clock.clone()));
    let caller = Caller::new(seed.user_id, seed.org_id);
    let owner = resolve_user_colleague(pool, seed.org_id, seed.user_id)
        .await
        .expect("owner colleague");

    let channel = ChannelId::new();
    sqlx::query(
        "INSERT INTO channels (id, org_id, name, created_by_user_id, created_at) \
         VALUES ($1, $2, 'room', $3, now())",
    )
    .bind(channel)
    .bind(seed.org_id)
    .bind(seed.user_id)
    .execute(pool)
    .await
    .expect("create channel");
    let thread = threads
        .create_thread(&caller, Some(channel), None, owner, None)
        .await
        .expect("create channel thread");

    let queue: SharedPromptQueue = Arc::new(PgPromptQueue::new(pool.clone(), clock.clone()));
    let dag: SharedDagBudget = Arc::new(PgDagBudget::new(pool.clone()));
    let sink: SharedResponseSink = Arc::new(PgResponseHub::new(pool.clone(), clock.clone()));
    let colleagues: SharedColleagueStore = Arc::new(PgColleagueStore::new(pool.clone()));
    let root = enqueue_root(&queue, &threads, pool, seed, thread).await;
    let state = threads
        .resolve_participation(&caller, thread, seed.agent_id)
        .await
        .expect("participation");
    let tool = SendMessageTool::new(
        threads.clone(),
        queue,
        dag,
        colleagues,
        sink,
        Arc::new(patom::outbound::NoopOutboundRouter),
    );

    let ctx = ToolCallContext {
        claim_key: patom::runtime::ClaimKey::from(state.as_uuid()),
        thread_id: Some(thread),
        state_id: Some(state),
        viewer: agent_participant(pool, seed.org_id, seed.agent_id).await,
        root_request_id: root,
        request_id: root,
        kind_payload: RequestKindPayload::Normal {},
        acting_user_id: seed.user_id,
        org_id: seed.org_id,
    };
    (tool, ctx, thread)
}

/// Enqueue a root trigger for the seed agent in `thread` so agent→agent
/// `send_message` has a real DAG budget row to debit.
async fn enqueue_root(
    queue: &SharedPromptQueue,
    threads: &SharedThreadStore,
    pool: &PgPool,
    seed: &common::pg::Seed,
    thread: patom::threads::ThreadId,
) -> PromptRequestId {
    let caller = Caller::new(seed.user_id, seed.org_id);
    let human = resolve_user_colleague(pool, seed.org_id, seed.user_id)
        .await
        .expect("human colleague");
    let state = threads
        .resolve_participation(&caller, thread, seed.agent_id)
        .await
        .expect("participation");
    queue
        .enqueue_trigger(patom::runtime::NewTrigger {
            org_id: seed.org_id,
            acting_user_id: seed.user_id,
            thread_id: Some(thread),
            state_id: Some(state),
            background_turn_id: None,
            sender_colleague_id: human,
            receiver_agent_id: seed.agent_id,
            root_request_id: None,
            trigger_message_id: None,
            idempotency_key: patom::runtime::IdempotencyKey::try_from(format!(
                "root-{}",
                thread.as_uuid()
            ))
            .expect("key"),
            kind_payload: RequestKindPayload::Normal {},
        })
        .await
        .expect("enqueue root")
}

// --- `to` target (#178) -----------------------------------------------------

/// Build a `send_message` tool wired with real PG stores + a noop router.
fn build_tool(pool: &PgPool, clock: &patom::clock::SharedClock) -> SendMessageTool {
    SendMessageTool::new(
        Arc::new(PgThreadStore::new(pool.clone(), clock.clone())),
        Arc::new(PgPromptQueue::new(pool.clone(), clock.clone())),
        Arc::new(PgDagBudget::new(pool.clone())),
        Arc::new(PgColleagueStore::new(pool.clone())),
        Arc::new(PgResponseHub::new(pool.clone(), clock.clone())),
        Arc::new(patom::outbound::NoopOutboundRouter),
    )
}

/// A `ToolCallContext` for `seed.agent`, with a real `prompt_requests` row so a
/// posted feed row satisfies its FK. `thread_id` is `None` — a `to` target does
/// not read the running thread.
async fn agent_ctx(pool: &PgPool, seed: &common::pg::Seed) -> ToolCallContext {
    let state = seed_agent_thread_state(pool, seed.org_id, seed.agent_id).await;
    let request_id =
        seed_prompt_request(pool, state, seed.agent_id, seed.org_id, seed.user_id).await;
    ToolCallContext {
        claim_key: patom::runtime::ClaimKey::from(state.as_uuid()),
        thread_id: None,
        state_id: Some(state),
        viewer: agent_participant(pool, seed.org_id, seed.agent_id).await,
        root_request_id: request_id,
        request_id,
        kind_payload: RequestKindPayload::Normal {},
        acting_user_id: seed.user_id,
        org_id: seed.org_id,
    }
}

async fn make_channel(pool: &PgPool, seed: &common::pg::Seed, name: &str) -> ChannelId {
    let channel = ChannelId::new();
    sqlx::query(
        "INSERT INTO channels (id, org_id, name, created_by_user_id, created_at) \
         VALUES ($1, $2, $3, $4, now())",
    )
    .bind(channel)
    .bind(seed.org_id)
    .bind(name)
    .bind(seed.user_id)
    .execute(pool)
    .await
    .expect("create channel");
    channel
}

async fn add_agent_member(
    pool: &PgPool,
    seed: &common::pg::Seed,
    channel: ChannelId,
    agent: ColleagueId,
) {
    sqlx::query(
        "INSERT INTO channel_agent_members (channel_id, colleague_id, org_id, added_at) \
         VALUES ($1, $2, $3, now())",
    )
    .bind(channel)
    .bind(agent)
    .bind(seed.org_id)
    .execute(pool)
    .await
    .expect("add agent member");
}

#[sqlx::test]
async fn to_channel_member_starts_new_thread(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let clock = SystemClock::shared();
    let agent_colleague = resolve_agent_colleague(&pool, seed.org_id, seed.agent_id)
        .await
        .expect("agent colleague");
    let channel = make_channel(&pool, &seed, "eng-room").await;
    add_agent_member(&pool, &seed, channel, agent_colleague).await;

    let tool = build_tool(&pool, &clock);
    let ctx = agent_ctx(&pool, &seed).await;
    tool.execute(
        json!({ "content": "kicking off here", "to": { "channel": channel } }),
        &ctx,
    )
    .await
    .expect("member posts to channel");

    let (threads_in_channel,): (i64,) =
        sqlx::query_as("SELECT count(*) FROM threads WHERE channel_id = $1")
            .bind(channel)
            .fetch_one(&pool)
            .await
            .expect("count threads");
    assert_eq!(
        threads_in_channel, 1,
        "a channel target starts one new thread"
    );

    let (posted,): (i64,) = sqlx::query_as(
        "SELECT count(*) FROM thread_messages m JOIN threads t ON t.id = m.thread_id \
         WHERE t.channel_id = $1 AND m.kind = 'posted'",
    )
    .bind(channel)
    .fetch_one(&pool)
    .await
    .expect("count posted");
    assert_eq!(posted, 1, "the message landed in the new channel thread");
}

#[sqlx::test]
async fn to_channel_non_member_rejected(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let clock = SystemClock::shared();
    // A channel the agent is NOT a member of.
    let channel = make_channel(&pool, &seed, "secret-room").await;

    let tool = build_tool(&pool, &clock);
    let ctx = agent_ctx(&pool, &seed).await;
    let err = tool
        .execute(
            json!({ "content": "let me in", "to": { "channel": channel } }),
            &ctx,
        )
        .await
        .expect_err("non-member agent is rejected");
    assert!(
        err.to_string().contains("not a member"),
        "rejection names the membership gate, got: {err}"
    );

    let (threads_in_channel,): (i64,) =
        sqlx::query_as("SELECT count(*) FROM threads WHERE channel_id = $1")
            .bind(channel)
            .fetch_one(&pool)
            .await
            .expect("count threads");
    assert_eq!(threads_in_channel, 0, "no thread created on rejection");
}

#[sqlx::test]
async fn to_dm_human_creates_dm_thread(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let clock = SystemClock::shared();
    let human = resolve_user_colleague(&pool, seed.org_id, seed.user_id)
        .await
        .expect("human colleague");

    let tool = build_tool(&pool, &clock);
    let ctx = agent_ctx(&pool, &seed).await;
    tool.execute(
        json!({ "content": "a quick question", "to": { "dm": human } }),
        &ctx,
    )
    .await
    .expect("dm to a human");

    let (dm_threads,): (i64,) = sqlx::query_as(
        "SELECT count(*) FROM threads WHERE channel_id IS NULL \
           AND dm_counterpart_colleague_id = $1",
    )
    .bind(human)
    .fetch_one(&pool)
    .await
    .expect("count dm threads");
    assert_eq!(
        dm_threads, 1,
        "a dm target opens one DM thread to the human"
    );
}

#[sqlx::test]
async fn to_dm_agent_rejected(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let clock = SystemClock::shared();
    // A second agent — DM targets must be humans.
    let other_agent = seed_agent(&pool, seed.org_id, "peer-bot").await;
    let other_colleague = resolve_agent_colleague(&pool, seed.org_id, other_agent)
        .await
        .expect("other agent colleague");

    let tool = build_tool(&pool, &clock);
    let ctx = agent_ctx(&pool, &seed).await;
    let err = tool
        .execute(
            json!({ "content": "hey peer", "to": { "dm": other_colleague } }),
            &ctx,
        )
        .await
        .expect_err("an agent dm target is rejected");
    assert!(
        err.to_string().contains("must be a human"),
        "rejection names the human-only DM rule, got: {err}"
    );
}
