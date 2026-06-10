//! P7: the channel feed is scoped by **membership**, not by who created the
//! thread or who is the "active" user.
//!
//! Two members of a channel both see its threads even though only one created
//! them; a non-member sees none. This is the read-side counterpart to the
//! `send_message` human gate — channel threads are shared among members, DMs
//! stay private to their participant.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::sync::Arc;

use patom::auth::{Caller, OrgId, UserId};
use patom::channels::ChannelId;
use patom::clock::SystemClock;
use patom::colleagues::{ColleagueId, resolve_agent_colleague, resolve_user_colleague};
use patom::threads::{PgThreadStore, SharedThreadStore, ThreadScope};
use sqlx::PgPool;

mod common;
use common::pg::seed_tenant;

/// Add a human to the org (mints their colleague via the org-members trigger).
async fn seed_human(pool: &PgPool, org_id: OrgId) -> (UserId, ColleagueId) {
    let user_id = UserId::new();
    sqlx::query(
        "INSERT INTO users (id, email, display_name, created_at, updated_at) \
         VALUES ($1, $2, 'Member', now(), now())",
    )
    .bind(user_id)
    .bind(format!(
        "member-{}@example.test",
        user_id.as_uuid().simple()
    ))
    .execute(pool)
    .await
    .expect("insert user");
    sqlx::query(
        "INSERT INTO org_members (org_id, user_id, role, created_at) VALUES ($1, $2, 'member', now())",
    )
    .bind(org_id)
    .bind(user_id)
    .execute(pool)
    .await
    .expect("insert membership");
    let colleague = resolve_user_colleague(pool, org_id, user_id)
        .await
        .expect("colleague");
    (user_id, colleague)
}

async fn add_channel_member(pool: &PgPool, channel: ChannelId, org: OrgId, user: UserId) {
    sqlx::query(
        "INSERT INTO channel_members (channel_id, user_id, org_id, added_at) \
         VALUES ($1, $2, $3, now()) ON CONFLICT DO NOTHING",
    )
    .bind(channel)
    .bind(user)
    .bind(org)
    .execute(pool)
    .await
    .expect("add channel member");
}

#[sqlx::test]
async fn channel_feed_scoped_to_membership_not_active_user(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let clock = SystemClock::shared();
    let store: SharedThreadStore = Arc::new(PgThreadStore::new(pool.clone(), clock));

    // User A (the seeded owner) creates the channel + a thread in it.
    let user_a = seed.user_id;
    let col_a = resolve_user_colleague(&pool, seed.org_id, user_a)
        .await
        .expect("col a");
    // Users B (member) and C (non-member, org member only).
    let (user_b, _col_b) = seed_human(&pool, seed.org_id).await;
    let (user_c, _col_c) = seed_human(&pool, seed.org_id).await;

    let channel = ChannelId::new();
    sqlx::query(
        "INSERT INTO channels (id, org_id, name, created_by_user_id, created_at) \
         VALUES ($1, $2, 'team-room', $3, now())",
    )
    .bind(channel)
    .bind(seed.org_id)
    .bind(user_a)
    .execute(&pool)
    .await
    .expect("create channel");
    add_channel_member(&pool, channel, seed.org_id, user_a).await;
    add_channel_member(&pool, channel, seed.org_id, user_b).await;
    // C is deliberately NOT added to the channel.

    let caller_a = Caller::new(user_a, seed.org_id);
    let thread = store
        .create_thread(&caller_a, Some(channel), None, col_a, None)
        .await
        .expect("A creates channel thread");

    // B is a member but NOT the creator — must still see A's thread.
    let caller_b = Caller::new(user_b, seed.org_id);
    let for_b = store
        .list_threads(&caller_b, ThreadScope::Channel(channel))
        .await
        .expect("list for B");
    assert!(
        for_b.iter().any(|t| t.thread_id == thread),
        "a channel member sees threads created by other members (scoped to membership, not creator)"
    );

    // C is not a member — must see nothing in this channel.
    let caller_c = Caller::new(user_c, seed.org_id);
    let for_c = store
        .list_threads(&caller_c, ThreadScope::Channel(channel))
        .await
        .expect("list for C");
    assert!(
        for_c.is_empty(),
        "a non-member sees no threads in the channel, got {}",
        for_c.len()
    );

    // DM view (channel_id = None) is private to its participant: A's DM is
    // visible to A, not to B.
    let agent_col = resolve_agent_colleague(&pool, seed.org_id, seed.agent_id)
        .await
        .expect("agent colleague");
    let dm = store
        .create_thread(&caller_a, None, None, col_a, Some(agent_col))
        .await
        .expect("A creates DM");
    let a_dms = store
        .list_threads(&caller_a, ThreadScope::Dms { counterpart: None })
        .await
        .expect("list A's DMs");
    assert!(
        a_dms.iter().any(|t| t.thread_id == dm),
        "the DM participant sees their own DM"
    );
    let b_dms = store
        .list_threads(&caller_b, ThreadScope::Dms { counterpart: None })
        .await
        .expect("list B's DMs");
    assert!(
        !b_dms.iter().any(|t| t.thread_id == dm),
        "a DM stays private — another user does not see it"
    );
}

/// A human↔human DM is visible to BOTH ends of the pair — the creator and the
/// counterpart — in both list and feed scoping, while a third org member sees
/// nothing. (Pre-counterpart, a DM was creator-only and human↔human DMs were
/// unrepresentable.)
#[sqlx::test]
async fn dm_counterpart_sees_thread_creator_and_back(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let clock = SystemClock::shared();
    let store: SharedThreadStore = Arc::new(PgThreadStore::new(pool.clone(), clock));

    let user_a = seed.user_id;
    let col_a = resolve_user_colleague(&pool, seed.org_id, user_a)
        .await
        .expect("col a");
    let (user_b, col_b) = seed_human(&pool, seed.org_id).await;
    let (user_c, _col_c) = seed_human(&pool, seed.org_id).await;

    let caller_a = Caller::new(user_a, seed.org_id);
    let caller_b = Caller::new(user_b, seed.org_id);
    let caller_c = Caller::new(user_c, seed.org_id);

    // A starts a DM with B.
    let dm = store
        .create_thread(&caller_a, None, None, col_a, Some(col_b))
        .await
        .expect("A starts a DM with B");

    // Both ends list it…
    for (who, caller, counterpart) in [("A", &caller_a, col_b), ("B", &caller_b, col_a)] {
        let dms = store
            .list_threads(
                caller,
                ThreadScope::Dms {
                    counterpart: Some(counterpart),
                },
            )
            .await
            .expect("list DMs");
        assert!(
            dms.iter().any(|t| t.thread_id == dm),
            "{who} sees the pair's DM"
        );
        assert!(
            store.visible_to(caller, dm).await.expect("visible_to"),
            "{who} passes the visibility gate"
        );
    }

    // …a third org member sees neither the listing nor the thread.
    let c_dms = store
        .list_threads(&caller_c, ThreadScope::Dms { counterpart: None })
        .await
        .expect("list C's DMs");
    assert!(
        c_dms.iter().all(|t| t.thread_id != dm),
        "C must not list A↔B's DM"
    );
    assert!(
        !store.visible_to(&caller_c, dm).await.expect("visible_to"),
        "C must not pass the visibility gate"
    );
}
