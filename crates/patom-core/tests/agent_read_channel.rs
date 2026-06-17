//! #199: the `read_channel` tool — a membership-gated, bounded cross-thread
//! read. A member agent reads the channel's recent posts; a non-member is
//! refused; an empty window returns a benign note; a non-agent caller is
//! rejected. The gate is the safety boundary, so the refusal paths matter as
//! much as the happy path.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::sync::Arc;

use patom::channels::ChannelId;
use patom::clock::SystemClock;
use patom::colleagues::{ColleagueId, resolve_agent_colleague, resolve_user_colleague};
use patom::provider::{ChatMessage, UserContent};
use patom::runtime::{ClaimKey, PromptRequestId, RequestKindPayload};
use patom::threads::{PgThreadStore, SharedThreadStore, ThreadId};
use patom::tools::system::ReadChannelTool;
use patom::tools::{Tool, ToolCallContext, ToolError};
use serde_json::json;
use sqlx::PgPool;
use uuid::Uuid;

mod common;
use common::pg::{Seed, agent_participant, human_participant, seed_tenant};

async fn make_channel(pool: &PgPool, org: patom::auth::OrgId, name: &str) -> ChannelId {
    let id = ChannelId::new();
    sqlx::query(
        "INSERT INTO channels (id, org_id, name, created_by_user_id, created_at) \
         VALUES ($1, $2, $3, NULL, now())",
    )
    .bind(id)
    .bind(org)
    .bind(name)
    .execute(pool)
    .await
    .expect("create channel");
    id
}

async fn add_agent_member(pool: &PgPool, seed: &Seed, channel: ChannelId, agent: ColleagueId) {
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

async fn post(pool: &PgPool, seed: &Seed, channel: ChannelId, sender: ColleagueId, text: &str) {
    let thread = ThreadId::new();
    sqlx::query(
        "INSERT INTO threads \
           (id, org_id, channel_id, root_message_id, created_by_colleague_id, \
            dm_counterpart_colleague_id, created_at, last_activity_at) \
         VALUES ($1, $2, $3, NULL, $4, NULL, now(), now())",
    )
    .bind(thread)
    .bind(seed.org_id)
    .bind(channel)
    .bind(sender)
    .execute(pool)
    .await
    .expect("create thread");
    let body = serde_json::to_value(ChatMessage::User(vec![UserContent::Text(text.to_owned())]))
        .expect("serialize body");
    sqlx::query(
        "INSERT INTO thread_messages \
           (id, thread_id, seq, kind, sender_colleague_id, owner_agent_id, \
            receiver_colleague_id, body, request_id, org_id, created_at, idempotency_key) \
         VALUES ($1, $2, 1, 'posted', $3, NULL, NULL, $4, NULL, $5, now(), NULL)",
    )
    .bind(Uuid::new_v4())
    .bind(thread)
    .bind(sender)
    .bind(&body)
    .bind(seed.org_id)
    .execute(pool)
    .await
    .expect("insert posted row");
}

fn store(pool: &PgPool) -> SharedThreadStore {
    Arc::new(PgThreadStore::new(pool.clone(), SystemClock::shared()))
}

fn agent_ctx(viewer: patom::types::Participant, seed: &Seed) -> ToolCallContext {
    let request_id = PromptRequestId::new();
    ToolCallContext {
        claim_key: ClaimKey::new(),
        thread_id: None,
        state_id: None,
        viewer,
        root_request_id: request_id,
        request_id,
        kind_payload: RequestKindPayload::Normal {},
        acting_user_id: seed.user_id,
        org_id: seed.org_id,
    }
}

#[sqlx::test]
async fn member_reads_recent_posts(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let agent = resolve_agent_colleague(&pool, seed.org_id, seed.agent_id)
        .await
        .expect("agent colleague");
    let human = resolve_user_colleague(&pool, seed.org_id, seed.user_id)
        .await
        .expect("human colleague");

    let chan = make_channel(&pool, seed.org_id, "engineering").await;
    add_agent_member(&pool, &seed, chan, agent).await;
    post(
        &pool,
        &seed,
        chan,
        human,
        "deploy is blocked on the migration",
    )
    .await;

    let tool = ReadChannelTool::new(store(&pool));
    let ctx = agent_ctx(
        agent_participant(&pool, seed.org_id, seed.agent_id).await,
        &seed,
    );
    let out = tool
        .execute(json!({ "channel": chan.as_uuid() }), &ctx)
        .await
        .expect("member read");
    assert!(
        out.contains("deploy is blocked on the migration"),
        "the transcript carries the channel's recent post, got: {out}"
    );
}

#[sqlx::test]
async fn non_member_is_refused(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let human = resolve_user_colleague(&pool, seed.org_id, seed.user_id)
        .await
        .expect("human colleague");
    // A channel the agent is NOT a member of, with traffic in it.
    let chan = make_channel(&pool, seed.org_id, "secret-room").await;
    post(&pool, &seed, chan, human, "classified").await;

    let tool = ReadChannelTool::new(store(&pool));
    let ctx = agent_ctx(
        agent_participant(&pool, seed.org_id, seed.agent_id).await,
        &seed,
    );
    let err = tool
        .execute(json!({ "channel": chan.as_uuid() }), &ctx)
        .await
        .expect_err("non-member must be refused");
    assert!(
        matches!(err, ToolError::InvalidInput(m) if m.contains("not a member")),
        "refusal is a model-correctable invalid_input"
    );
}

#[sqlx::test]
async fn empty_window_returns_benign_note(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let agent = resolve_agent_colleague(&pool, seed.org_id, seed.agent_id)
        .await
        .expect("agent colleague");
    let chan = make_channel(&pool, seed.org_id, "quiet-room").await;
    add_agent_member(&pool, &seed, chan, agent).await;

    let tool = ReadChannelTool::new(store(&pool));
    let ctx = agent_ctx(
        agent_participant(&pool, seed.org_id, seed.agent_id).await,
        &seed,
    );
    let out = tool
        .execute(json!({ "channel": chan.as_uuid() }), &ctx)
        .await
        .expect("member read of an empty channel is not an error");
    assert!(out.contains("no messages"), "empty window is a benign note");
}

#[sqlx::test]
async fn non_agent_caller_is_rejected(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let agent = resolve_agent_colleague(&pool, seed.org_id, seed.agent_id)
        .await
        .expect("agent colleague");
    let chan = make_channel(&pool, seed.org_id, "engineering").await;
    add_agent_member(&pool, &seed, chan, agent).await;

    // A human viewer has a colleague id but no agent id — humans don't run tools.
    let tool = ReadChannelTool::new(store(&pool));
    let ctx = agent_ctx(
        human_participant(&pool, seed.org_id, seed.user_id).await,
        &seed,
    );
    let err = tool
        .execute(json!({ "channel": chan.as_uuid() }), &ctx)
        .await
        .expect_err("a non-agent caller must be rejected");
    assert!(
        matches!(err, ToolError::InvalidInput(m) if m.contains("agent")),
        "non-agent caller rejected with invalid_input"
    );
}
