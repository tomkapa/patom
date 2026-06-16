//! Discord pre-join history backfill (D5.5).
//!
//! On the first inbound message in a channel, the bridge runs a one-shot
//! backfill: it reads the channel's older messages over REST (a `FakeHistoryReader`
//! here) and mirrors them into the same Patom thread *before* the live message,
//! so the agent reads the whole conversation in chronological order — not just
//! from the moment it joined. It runs at most once per channel.

#![allow(clippy::expect_used)]

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde_json::json;
use sqlx::PgPool;

use patom::auth::{Caller, run_privileged};
use patom::clock::{SharedClock, SystemClock};
use patom::colleagues::PgColleagueStore;
use patom::crypto::OrgEncryptor;
use patom::discord::app_store::{NewDiscordApp, PgDiscordAppStore, SharedDiscordAppStore};
use patom::discord::bridge::{
    self, AttachRequest, BridgeDeps, OutboundAttach, SharedOutboundAttach,
};
use patom::discord::channel_map::PgDiscordChannelStore;
use patom::discord::connection::InboundDispatch;
use patom::discord::directory::PgDiscordDirectory;
use patom::discord::event::InboundMessage;
use patom::discord::history::FakeHistoryReader;
use patom::discord::thread_map::PgDiscordThreadStore;
use patom::discord::thread_opener::{FakeThreadOpener, SharedThreadOpener};
use patom::discord::types::{ApplicationId, BotToken, ContainerId, DiscordUserId};
use patom::runtime::PgPromptQueue;
use patom::threads::PgThreadStore;

mod common;
use common::pg::seed_tenant;

const APP_ID: &str = "111111111111111111";
const BOT_USER_ID: &str = "999999999999999999";
const GUILD_ID: &str = "222222222222222222";
const CHANNEL_ID: &str = "333333333333333333";

#[derive(Debug, Default)]
struct NoopAttach(Mutex<usize>);

#[async_trait]
impl OutboundAttach for NoopAttach {
    async fn attach(&self, _req: AttachRequest) {
        *self.0.lock().expect("lock") += 1;
    }
}

/// A history message object (the REST shape — same as a MESSAGE_CREATE `d`).
fn hist_msg(id: &str, author_id: &str, content: &str) -> InboundMessage {
    serde_json::from_value(json!({
        "id": id,
        "channel_id": CHANNEL_ID,
        "author": {"id": author_id, "username": format!("user{author_id}")},
        "content": content,
    }))
    .expect("history message")
}

/// A live ambient (no-mention) guild message — triggers backfill on first sight
/// but does not run the agent.
fn ambient(msg_id: &str, content: &str) -> InboundDispatch {
    InboundDispatch {
        application_id: ApplicationId::try_from(APP_ID).expect("app id"),
        bot_user_id: DiscordUserId::try_from(BOT_USER_ID).expect("bot id"),
        event_type: "MESSAGE_CREATE".to_owned(),
        data: json!({
            "id": msg_id,
            "channel_id": CHANNEL_ID,
            "guild_id": GUILD_ID,
            "author": {"id": "444444444444444444", "username": "alice", "global_name": "Alice", "bot": false},
            "content": content,
            "mentions": [],
        }),
    }
}

async fn build_deps(
    pool: &PgPool,
    caller: &Caller,
    agent_id: patom::agents::AgentId,
    history: Arc<FakeHistoryReader>,
) -> BridgeDeps {
    let clock: SharedClock = SystemClock::shared();
    let enc = Arc::new(OrgEncryptor::for_test([7u8; 32]));
    let app_store = Arc::new(PgDiscordAppStore::new(pool.clone(), clock.clone(), enc));
    let apps: SharedDiscordAppStore = app_store;
    apps.register(
        caller,
        NewDiscordApp {
            application_id: ApplicationId::try_from(APP_ID).expect("app id"),
            agent_id,
            bot_token: BotToken::try_from("MTk4N.example.token".to_owned()).expect("token"),
        },
    )
    .await
    .expect("register");

    let outbound: SharedOutboundAttach = Arc::new(NoopAttach::default());
    // Ambient (no-mention) messages never open a thread, so the opener is unused.
    let thread_opener: SharedThreadOpener = Arc::new(FakeThreadOpener::returning(
        ContainerId::try_from("555000000000000001").expect("thread id"),
    ));
    BridgeDeps {
        apps,
        directory: Arc::new(PgDiscordDirectory::new(pool.clone(), clock.clone())),
        channels: Arc::new(PgDiscordChannelStore::new(pool.clone(), clock.clone())),
        threads: Arc::new(PgDiscordThreadStore::new(pool.clone(), clock.clone())),
        thread_store: Arc::new(PgThreadStore::new(pool.clone(), clock.clone())),
        colleagues: Arc::new(PgColleagueStore::new(pool.clone())),
        queue: Arc::new(PgPromptQueue::new(pool.clone(), clock.clone())),
        outbound,
        history,
        thread_opener,
    }
}

async fn count(pool: &PgPool, sql: &'static str) -> i64 {
    run_privileged::<i64, sqlx::Error>(pool, async |tx| {
        sqlx::query_scalar(sql).fetch_one(&mut **tx).await
    })
    .await
    .expect("count")
}

/// The `posted` message texts in thread order (oldest-first by seq).
async fn texts_in_order(pool: &PgPool) -> Vec<String> {
    run_privileged::<Vec<(String,)>, sqlx::Error>(pool, async |tx| {
        sqlx::query_as(
            "SELECT body->'contents'->0->>'value' \
               FROM thread_messages WHERE kind = 'posted' ORDER BY seq ASC",
        )
        .fetch_all(&mut **tx)
        .await
    })
    .await
    .expect("texts")
    .into_iter()
    .map(|(t,)| t)
    .collect()
}

#[sqlx::test]
async fn first_message_backfills_pre_join_history_in_order(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let caller = Caller::new(seed.user_id, seed.org_id);
    // Discord returns history newest-first: [30, 20, 10].
    let history = Arc::new(FakeHistoryReader::with_pages(vec![vec![
        hist_msg("30", "502", "third"),
        hist_msg("20", "501", "second"),
        hist_msg("10", "501", "first"),
    ]]));
    let deps = build_deps(&pool, &caller, seed.agent_id, history).await;

    // The live (most recent) message arrives → backfill fires on first sight.
    bridge::process_event(&deps, ambient("40", "live now"))
        .await
        .expect("process");

    // 3 backfilled + 1 live = 4 posted rows, in chronological order.
    assert_eq!(
        texts_in_order(&pool).await,
        vec![
            "first".to_owned(),
            "second".to_owned(),
            "third".to_owned(),
            "live now".to_owned(),
        ],
        "backfilled history precedes the live message, oldest-first",
    );
    // The two distinct backfilled authors + the live author are shadow-minted.
    assert_eq!(
        count(&pool, "SELECT COUNT(*) FROM discord_user_handles").await,
        3
    );
    // The one-shot is marked complete.
    assert_eq!(
        count(
            &pool,
            "SELECT COUNT(*) FROM discord_threads WHERE backfill_complete"
        )
        .await,
        1,
    );
    // Backfill is context only — no trigger enqueued for an ambient message.
    assert_eq!(
        count(&pool, "SELECT COUNT(*) FROM prompt_requests").await,
        0
    );
}

#[sqlx::test]
async fn backfill_runs_at_most_once_per_channel(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let caller = Caller::new(seed.user_id, seed.org_id);
    // Both pages are non-empty; if backfill ran twice, the 2nd page would mirror.
    let history = Arc::new(FakeHistoryReader::with_pages(vec![
        vec![hist_msg("10", "501", "history")],
        vec![hist_msg("99", "777", "must-not-appear")],
    ]));
    let deps = build_deps(&pool, &caller, seed.agent_id, history).await;

    bridge::process_event(&deps, ambient("40", "one"))
        .await
        .expect("first");
    bridge::process_event(&deps, ambient("41", "two"))
        .await
        .expect("second");

    // history(1) + live "one" + live "two" = 3 — the second page never ran.
    assert_eq!(
        texts_in_order(&pool).await,
        vec!["history".to_owned(), "one".to_owned(), "two".to_owned()],
    );
    assert_eq!(
        count(
            &pool,
            "SELECT COUNT(*) FROM thread_messages WHERE body::text LIKE '%must-not-appear%'"
        )
        .await,
        0,
        "the second message did not re-run backfill",
    );
}
