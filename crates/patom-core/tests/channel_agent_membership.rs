//! Stage F (issue #178): colleague-keyed channel membership.
//!
//! `colleague_in_channel` is the authority check for an agent addressing a
//! channel; `channels_for_colleague` feeds the `<channels>` context block. Both
//! union human (`channel_members`) and agent (`channel_agent_members`)
//! membership.

#![allow(clippy::expect_used)]

use patom::channels::ChannelId;
use patom::clock::SystemClock;
use patom::colleagues::{resolve_agent_colleague, resolve_user_colleague};
use patom::threads::{PgThreadStore, ThreadStore};
use sqlx::PgPool;

mod common;
use common::pg::seed_tenant;

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

#[sqlx::test]
async fn agent_membership_gate_and_listing(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let clock = SystemClock::shared();
    let store = PgThreadStore::new(pool.clone(), clock);

    let agent = resolve_agent_colleague(&pool, seed.org_id, seed.agent_id)
        .await
        .expect("agent colleague");
    let joined = make_channel(&pool, seed.org_id, "joined-room").await;
    let other = make_channel(&pool, seed.org_id, "other-room").await;

    sqlx::query(
        "INSERT INTO channel_agent_members (channel_id, colleague_id, org_id, added_at) \
         VALUES ($1, $2, $3, now())",
    )
    .bind(joined)
    .bind(agent)
    .bind(seed.org_id)
    .execute(&pool)
    .await
    .expect("add agent member");

    assert!(
        store
            .colleague_in_channel(joined, agent)
            .await
            .expect("check"),
        "agent is a member of the channel it was added to"
    );
    assert!(
        !store
            .colleague_in_channel(other, agent)
            .await
            .expect("check"),
        "agent is not a member of an unrelated channel"
    );

    let channels = store
        .channels_for_colleague(seed.org_id, agent)
        .await
        .expect("list");
    let ids: Vec<ChannelId> = channels.iter().map(|c| c.id).collect();
    assert!(ids.contains(&joined), "listing includes the joined channel");
    assert!(
        !ids.contains(&other),
        "listing excludes a channel the agent is not in"
    );
}

#[sqlx::test]
async fn human_membership_resolves_through_colleague(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let clock = SystemClock::shared();
    let store = PgThreadStore::new(pool.clone(), clock);

    // The org owner is auto-enrolled in `#general` by the org_members trigger.
    let (general,): (ChannelId,) =
        sqlx::query_as("SELECT id FROM channels WHERE org_id = $1 AND name = 'general'")
            .bind(seed.org_id)
            .fetch_one(&pool)
            .await
            .expect("general");
    let human = resolve_user_colleague(&pool, seed.org_id, seed.user_id)
        .await
        .expect("human colleague");

    assert!(
        store
            .colleague_in_channel(general, human)
            .await
            .expect("check"),
        "a human's channel_members row resolves through their colleague"
    );
    let channels = store
        .channels_for_colleague(seed.org_id, human)
        .await
        .expect("list");
    assert!(
        channels.iter().any(|c| c.id == general),
        "the human's channels include #general"
    );
}
