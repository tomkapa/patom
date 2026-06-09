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
use patom::colleagues::{PgColleagueStore, SharedColleagueStore, resolve_user_colleague};
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
use common::pg::{agent_participant, seed_tenant};

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
        .create_thread(&caller, Some(channel), None, owner_colleague)
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
    let agents = common::pg::shared_agent_store(pool.clone(), clock.clone());
    let tool = SendMessageTool::new(threads.clone(), queue, dag, agents, colleagues, sink);

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
                "receiver": { "kind": "colleague", "id": outsider_colleague },
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
