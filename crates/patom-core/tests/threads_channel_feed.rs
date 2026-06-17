//! #199: `ThreadStore::channel_feed` — the channel-level history read backing
//! the `read_channel` digest tool.
//!
//! A channel's recent `posted` chat lives across many Patom threads (the ambient
//! channel thread + every @mention sub-thread), all sharing `threads.channel_id`.
//! `channel_feed` unions them into one bounded, oldest-first slice: `since`-
//! filtered, `limit`-capped, body preview-capped in SQL, and isolated per
//! channel. Rows are inserted directly (as the live bridge mirror does) with
//! explicit `created_at`s so the read is exercised in isolation from the clock.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use chrono::{DateTime, TimeZone, Utc};
use patom::channels::ChannelId;
use patom::clock::SystemClock;
use patom::colleagues::{ColleagueId, resolve_user_colleague};
use patom::provider::{ChatMessage, UserContent};
use patom::threads::{PgThreadStore, READ_CHANNEL_BODY_MAX_CHARS, ThreadId, ThreadStore};
use sqlx::PgPool;
use uuid::Uuid;

mod common;
use common::pg::{Seed, seed_tenant};

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

async fn make_thread(
    pool: &PgPool,
    seed: &Seed,
    channel: ChannelId,
    creator: ColleagueId,
) -> ThreadId {
    let id = ThreadId::new();
    sqlx::query(
        "INSERT INTO threads \
           (id, org_id, channel_id, root_message_id, created_by_colleague_id, \
            dm_counterpart_colleague_id, created_at, last_activity_at) \
         VALUES ($1, $2, $3, NULL, $4, NULL, now(), now())",
    )
    .bind(id)
    .bind(seed.org_id)
    .bind(channel)
    .bind(creator)
    .execute(pool)
    .await
    .expect("create thread");
    id
}

/// Insert one `posted` row directly (the shape the live mirror writes), at an
/// explicit `created_at`. `sender = None` writes the synthetic System sender.
#[allow(clippy::too_many_arguments)]
async fn post_at(
    pool: &PgPool,
    seed: &Seed,
    thread: ThreadId,
    seq: i64,
    sender: Option<ColleagueId>,
    text: &str,
    created_at: DateTime<Utc>,
) {
    let body = serde_json::to_value(ChatMessage::User(vec![UserContent::Text(text.to_owned())]))
        .expect("serialize body");
    sqlx::query(
        "INSERT INTO thread_messages \
           (id, thread_id, seq, kind, sender_colleague_id, owner_agent_id, \
            receiver_colleague_id, body, request_id, org_id, created_at, idempotency_key) \
         VALUES ($1, $2, $3, 'posted', $4, NULL, NULL, $5, NULL, $6, $7, NULL)",
    )
    .bind(Uuid::new_v4())
    .bind(thread)
    .bind(seq)
    .bind(sender)
    .bind(&body)
    .bind(seed.org_id)
    .bind(created_at)
    .execute(pool)
    .await
    .expect("insert posted row");
}

fn at(h: u32, m: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 6, 17, h, m, 0)
        .single()
        .expect("unambiguous")
}

/// Shared fixture: a digest channel with two threads (ambient + a mention
/// sub-thread) carrying three human posts at 09:00 / 09:30 / 10:00, plus one
/// post in an unrelated channel. Returns the store + the digest channel id.
async fn seeded_channel(pool: &PgPool) -> (PgThreadStore, ChannelId) {
    let seed = seed_tenant(pool).await;
    let store = PgThreadStore::new(pool.clone(), SystemClock::shared());
    let human = resolve_user_colleague(pool, seed.org_id, seed.user_id)
        .await
        .expect("human colleague");

    let chan = make_channel(pool, seed.org_id, "engineering").await;
    let other = make_channel(pool, seed.org_id, "random").await;
    let t1 = make_thread(pool, &seed, chan, human).await;
    let t2 = make_thread(pool, &seed, chan, human).await;
    let t_other = make_thread(pool, &seed, other, human).await;

    post_at(pool, &seed, t1, 1, Some(human), "earliest", at(9, 0)).await;
    post_at(pool, &seed, t2, 1, Some(human), "middle", at(9, 30)).await;
    post_at(pool, &seed, t1, 2, Some(human), "latest", at(10, 0)).await;
    post_at(pool, &seed, t_other, 1, Some(human), "elsewhere", at(9, 45)).await;

    (store, chan)
}

#[sqlx::test]
async fn unions_channel_threads_chronologically_excluding_other_channels(pool: PgPool) {
    let (store, chan) = seeded_channel(&pool).await;
    let all = store
        .channel_feed(chan, None, 100)
        .await
        .expect("channel feed");
    let previews: Vec<&str> = all.iter().map(|r| r.body_preview.as_str()).collect();
    assert_eq!(
        previews,
        vec!["earliest", "middle", "latest"],
        "unions both threads in chronological order, excludes the other channel"
    );
}

#[sqlx::test]
async fn resolves_human_author_display_name(pool: PgPool) {
    let (store, chan) = seeded_channel(&pool).await;
    let all = store
        .channel_feed(chan, None, 100)
        .await
        .expect("channel feed");
    assert!(
        all.iter().all(|r| r.author.is_some()),
        "human-authored rows resolve a display name"
    );
}

#[sqlx::test]
async fn since_filter_keeps_rows_at_or_after_cursor(pool: PgPool) {
    let (store, chan) = seeded_channel(&pool).await;
    let recent = store
        .channel_feed(chan, Some(at(9, 30)), 100)
        .await
        .expect("since feed");
    let previews: Vec<&str> = recent.iter().map(|r| r.body_preview.as_str()).collect();
    assert_eq!(
        previews,
        vec!["middle", "latest"],
        "since filter keeps rows at/after the cursor"
    );
}

#[sqlx::test]
async fn limit_caps_to_the_newest_rows(pool: PgPool) {
    let (store, chan) = seeded_channel(&pool).await;
    let capped = store.channel_feed(chan, None, 1).await.expect("limit feed");
    assert_eq!(capped.len(), 1, "limit caps the page");
    assert_eq!(capped[0].body_preview, "latest", "keeps the newest row");
}

#[sqlx::test]
async fn oversized_body_is_preview_capped_in_sql(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let store = PgThreadStore::new(pool.clone(), SystemClock::shared());
    let human = resolve_user_colleague(&pool, seed.org_id, seed.user_id)
        .await
        .expect("human colleague");
    let chan = make_channel(&pool, seed.org_id, "verbose").await;
    let t = make_thread(&pool, &seed, chan, human).await;

    let cap = usize::try_from(READ_CHANNEL_BODY_MAX_CHARS).expect("cap fits usize");
    post_at(
        &pool,
        &seed,
        t,
        1,
        Some(human),
        &"x".repeat(cap * 3),
        at(9, 0),
    )
    .await;

    let rows = store
        .channel_feed(chan, None, 100)
        .await
        .expect("channel feed");
    assert_eq!(
        rows[0].body_preview.chars().count(),
        cap,
        "an oversized body is preview-capped in SQL, not shipped whole"
    );
}

#[sqlx::test]
async fn system_sender_row_has_no_author(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let store = PgThreadStore::new(pool.clone(), SystemClock::shared());
    let human = resolve_user_colleague(&pool, seed.org_id, seed.user_id)
        .await
        .expect("human colleague");
    let chan = make_channel(&pool, seed.org_id, "system-room").await;
    let t = make_thread(&pool, &seed, chan, human).await;

    // A System-authored row (NULL sender) — e.g. a synthetic note.
    post_at(&pool, &seed, t, 1, None, "system speaks", at(9, 0)).await;

    let rows = store
        .channel_feed(chan, None, 100)
        .await
        .expect("channel feed");
    assert!(
        rows[0].author.is_none(),
        "a System-sender row has no display name"
    );
}
