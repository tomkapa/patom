//! Discord auto-thread behavior — a top-level channel `@mention` opens a thread
//! and the agent converses there (keeping the channel clean), with **strict**
//! triggering: `@mention`-or-DM only, so a message inside the thread does *not*
//! auto-continue — a follow-up re-`@mentions` the bot.
//!
//! Like `discord_bridge.rs`, these drive `bridge::process_event` directly and
//! assert the inbound side only (no worker runs, so a shadow acting user can't
//! hang the pool on teardown). The thread-open seam is a [`FakeThreadOpener`].

#![allow(clippy::expect_used)]

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde_json::json;
use sqlx::PgPool;

use patom::auth::{Caller, run_privileged};
use patom::clock::{SharedClock, SystemClock};
use patom::colleagues::PgColleagueStore;
use patom::crypto::OrgEncryptor;
use patom::discord::app_store::{DiscordAppStore, NewDiscordApp, PgDiscordAppStore};
use patom::discord::bridge::{
    self, AttachRequest, BridgeDeps, OutboundAttach, SharedOutboundAttach,
};
use patom::discord::channel_map::PgDiscordChannelStore;
use patom::discord::connection::InboundDispatch;
use patom::discord::directory::PgDiscordDirectory;
use patom::discord::error::DiscordError;
use patom::discord::event::InboundMessage;
use patom::discord::history::{FakeHistoryReader, HistoryReader, SharedHistoryReader};
use patom::discord::thread_map::PgDiscordThreadStore;
use patom::discord::thread_opener::{FakeThreadOpener, SharedThreadOpener};
use patom::discord::types::{
    ApplicationId, BotToken, ContainerId, DiscordMessageId, DiscordUserId,
};
use patom::runtime::PgPromptQueue;
use patom::threads::PgThreadStore;

mod common;
use common::pg::seed_tenant;

const APP_ID: &str = "111111111111111111";
const BOT_USER_ID: &str = "999999999999999999";
const GUILD_ID: &str = "222222222222222222";
const CHANNEL_ID: &str = "333333333333333333";
const THREAD_ID: &str = "555000000000000001";

/// Records every outbound-pump attach the bridge requests.
#[derive(Debug, Default)]
struct FakeOutboundAttach {
    attached: Mutex<Vec<AttachRequest>>,
}

#[async_trait]
impl OutboundAttach for FakeOutboundAttach {
    async fn attach(&self, req: AttachRequest) {
        self.attached.lock().expect("attach mutex").push(req);
    }
}

/// Counts every `fetch_before` so a test can assert a bot-opened thread never
/// pages history.
#[derive(Debug, Default)]
struct CountingHistoryReader {
    calls: Mutex<usize>,
}

#[async_trait]
impl HistoryReader for CountingHistoryReader {
    async fn fetch_before(
        &self,
        _application_id: &ApplicationId,
        _container_id: &ContainerId,
        _before: &DiscordMessageId,
        _limit: usize,
    ) -> Result<Vec<InboundMessage>, DiscordError> {
        *self.calls.lock().expect("calls mutex") += 1;
        Ok(Vec::new())
    }
}

struct Rig {
    deps: BridgeDeps,
    outbound: Arc<FakeOutboundAttach>,
    opener: Arc<FakeThreadOpener>,
}

async fn build_rig(
    pool: &PgPool,
    caller: &Caller,
    agent_id: patom::agents::AgentId,
    opener: Arc<FakeThreadOpener>,
    history: SharedHistoryReader,
) -> Rig {
    let clock: SharedClock = SystemClock::shared();
    let enc = Arc::new(OrgEncryptor::for_test([7u8; 32]));
    let app_store = Arc::new(PgDiscordAppStore::new(pool.clone(), clock.clone(), enc));
    app_store
        .register(
            caller,
            NewDiscordApp {
                application_id: ApplicationId::try_from(APP_ID).expect("app id"),
                agent_id,
                bot_token: BotToken::try_from("MTk4N.example.token".to_owned()).expect("token"),
            },
        )
        .await
        .expect("register discord app");

    let outbound = Arc::new(FakeOutboundAttach::default());
    let outbound_seam: SharedOutboundAttach = outbound.clone();
    let thread_opener: SharedThreadOpener = opener.clone();
    let deps = BridgeDeps {
        apps: app_store,
        directory: Arc::new(PgDiscordDirectory::new(pool.clone(), clock.clone())),
        channels: Arc::new(PgDiscordChannelStore::new(pool.clone(), clock.clone())),
        threads: Arc::new(PgDiscordThreadStore::new(pool.clone(), clock.clone())),
        thread_store: Arc::new(PgThreadStore::new(pool.clone(), clock.clone())),
        colleagues: Arc::new(PgColleagueStore::new(pool.clone())),
        queue: Arc::new(PgPromptQueue::new(pool.clone(), clock.clone())),
        outbound: outbound_seam,
        history,
        thread_opener,
        assets: None,
        attachment_fetcher: Arc::new(patom::discord::attachment::FakeAttachmentFetcher::new()),
    };
    Rig {
        deps,
        outbound,
        opener,
    }
}

/// A guild MESSAGE_CREATE in `channel_id` (a top-level channel or a thread id),
/// optionally `@`-mentioning the bot.
fn guild_msg(msg_id: &str, channel_id: &str, content: &str, mentions: &[&str]) -> InboundDispatch {
    InboundDispatch {
        application_id: ApplicationId::try_from(APP_ID).expect("app id"),
        bot_user_id: DiscordUserId::try_from(BOT_USER_ID).expect("bot id"),
        event_type: "MESSAGE_CREATE".to_owned(),
        data: json!({
            "id": msg_id,
            "channel_id": channel_id,
            "guild_id": GUILD_ID,
            "author": {"id": "444444444444444444", "username": "alice", "global_name": "Alice", "bot": false},
            "content": content,
            "mentions": mentions.iter().map(|m| json!({"id": m})).collect::<Vec<_>>(),
        }),
    }
}

async fn count(pool: &PgPool, sql: &'static str) -> i64 {
    run_privileged::<i64, sqlx::Error>(pool, async |tx| {
        sqlx::query_scalar(sql).fetch_one(&mut **tx).await
    })
    .await
    .expect("count")
}

#[sqlx::test]
async fn channel_mention_opens_thread_and_continues_on_re_mention(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let caller = Caller::new(seed.user_id, seed.org_id);
    let opener = Arc::new(FakeThreadOpener::returning(
        ContainerId::try_from(THREAD_ID).expect("thread id"),
    ));
    let rig = build_rig(
        &pool,
        &caller,
        seed.agent_id,
        opener,
        Arc::new(FakeHistoryReader::empty()),
    )
    .await;

    // 1) Top-level channel @mention → the bot opens a thread and converses there.
    bridge::process_event(
        &rig.deps,
        guild_msg(
            "2001",
            CHANNEL_ID,
            "<@999999999999999999> draft a JD",
            &[BOT_USER_ID],
        ),
    )
    .await
    .expect("process channel mention");

    assert_eq!(rig.opener.call_count(), 1, "a thread was opened once");
    assert_eq!(
        count(
            &pool,
            "SELECT COUNT(*) FROM discord_threads \
             WHERE container_id = '555000000000000001' AND parent_id = '333333333333333333'"
        )
        .await,
        1,
        "the thread binds with its parent channel (is_thread)",
    );
    {
        let attached = rig.outbound.attached.lock().expect("lock");
        assert_eq!(attached.len(), 1);
        assert_eq!(
            attached[0].container_id.as_str(),
            THREAD_ID,
            "the reply routes into the opened thread",
        );
        assert!(
            attached[0].reply_to.is_none(),
            "no reference inside a fresh thread"
        );
    }

    // 2) A re-@mention INSIDE that thread (channel_id == the thread) continues
    //    the same Patom thread — no second thread is opened.
    bridge::process_event(
        &rig.deps,
        guild_msg(
            "2002",
            THREAD_ID,
            "<@999999999999999999> and a scorecard",
            &[BOT_USER_ID],
        ),
    )
    .await
    .expect("process re-mention in thread");

    assert_eq!(
        rig.opener.call_count(),
        1,
        "no second thread opened for a continuation"
    );
    assert_eq!(
        count(&pool, "SELECT COUNT(*) FROM discord_threads").await,
        1,
        "the re-mention reuses the same thread binding",
    );
    assert_eq!(
        count(&pool, "SELECT COUNT(*) FROM prompt_requests").await,
        2,
        "the re-mention is itself a trigger (strict @mention model)",
    );
    let attached = rig.outbound.attached.lock().expect("lock");
    assert_eq!(attached.len(), 2);
    assert_eq!(attached[1].container_id.as_str(), THREAD_ID);
    assert!(
        attached[1].reply_to.is_none(),
        "continuation posts plainly in the thread"
    );
}

#[sqlx::test]
async fn non_mention_in_owned_thread_does_not_trigger(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let caller = Caller::new(seed.user_id, seed.org_id);
    let opener = Arc::new(FakeThreadOpener::returning(
        ContainerId::try_from(THREAD_ID).expect("thread id"),
    ));
    let rig = build_rig(
        &pool,
        &caller,
        seed.agent_id,
        opener,
        Arc::new(FakeHistoryReader::empty()),
    )
    .await;

    // Open the thread with a channel mention (the one trigger).
    bridge::process_event(
        &rig.deps,
        guild_msg(
            "2101",
            CHANNEL_ID,
            "<@999999999999999999> hi",
            &[BOT_USER_ID],
        ),
    )
    .await
    .expect("open thread");

    // A plain (no-mention) message inside the bot-owned thread is INGESTED but
    // must NOT trigger — strict @mention/DM only, no auto-continue.
    bridge::process_event(
        &rig.deps,
        guild_msg("2102", THREAD_ID, "more context here", &[]),
    )
    .await
    .expect("ambient in thread");

    assert_eq!(
        count(&pool, "SELECT COUNT(*) FROM prompt_requests").await,
        1,
        "the no-mention thread message does not enqueue a second trigger",
    );
    assert_eq!(
        rig.opener.call_count(),
        1,
        "no thread opened for the ambient message"
    );
    // …but it WAS mirrored into the thread (ingest for context).
    assert_eq!(
        count(
            &pool,
            "SELECT COUNT(*) FROM thread_messages WHERE kind = 'posted'"
        )
        .await,
        2,
        "the mention and the ambient follow-up are both mirrored",
    );
    assert_eq!(
        rig.outbound.attached.lock().expect("lock").len(),
        1,
        "the pump is attached only for the trigger",
    );
}

#[sqlx::test]
async fn open_failure_falls_back_to_channel_reply(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let caller = Caller::new(seed.user_id, seed.org_id);
    // The opener fails (e.g. already a thread, a forum, missing perms).
    let rig = build_rig(
        &pool,
        &caller,
        seed.agent_id,
        Arc::new(FakeThreadOpener::failing()),
        Arc::new(FakeHistoryReader::empty()),
    )
    .await;

    bridge::process_event(
        &rig.deps,
        guild_msg(
            "2201",
            CHANNEL_ID,
            "<@999999999999999999> draft a JD",
            &[BOT_USER_ID],
        ),
    )
    .await
    .expect("process mention with failing opener");

    assert_eq!(rig.opener.call_count(), 1, "an open was attempted");
    // No thread binding; the conversation falls back to the channel itself.
    assert_eq!(
        count(
            &pool,
            "SELECT COUNT(*) FROM discord_threads WHERE container_id = '555000000000000001'"
        )
        .await,
        0,
        "no thread binding when the open failed",
    );
    assert_eq!(
        count(
            &pool,
            "SELECT COUNT(*) FROM discord_threads \
             WHERE container_id = '333333333333333333' \
               AND parent_id IS NULL AND is_thread = TRUE"
        )
        .await,
        1,
        "a permanent open failure records the container as a thread (parent unknown)",
    );
    // The trigger still fires, replying inline in the channel under the message.
    assert_eq!(
        count(&pool, "SELECT COUNT(*) FROM prompt_requests").await,
        1
    );
    let attached = rig.outbound.attached.lock().expect("lock");
    assert_eq!(attached.len(), 1);
    assert_eq!(
        attached[0].container_id.as_str(),
        CHANNEL_ID,
        "the fallback reply lands in the channel",
    );
    assert!(
        attached[0].reply_to.is_some(),
        "the fallback is an inline reply under the trigger",
    );
}

#[sqlx::test]
async fn repeated_mention_in_user_thread_does_not_retry_open(pool: PgPool) {
    // Simulates @mentioning the agent inside a manually-created Discord thread:
    // opening a thread-from-message there fails (Discord 50024, a 4xx), so we
    // converse in the thread AND remember it's non-threadable — a re-mention must
    // not re-attempt (and re-fail) the open.
    let seed = seed_tenant(&pool).await;
    let caller = Caller::new(seed.user_id, seed.org_id);
    let rig = build_rig(
        &pool,
        &caller,
        seed.agent_id,
        Arc::new(FakeThreadOpener::failing()),
        Arc::new(FakeHistoryReader::empty()),
    )
    .await;

    // First @mention: tries to open, gets a 4xx, falls back to the container.
    bridge::process_event(
        &rig.deps,
        guild_msg(
            "2401",
            CHANNEL_ID,
            "<@999999999999999999> hi",
            &[BOT_USER_ID],
        ),
    )
    .await
    .expect("first mention");
    assert_eq!(
        rig.opener.call_count(),
        1,
        "one open attempt on first sight"
    );

    // Second @mention in the same container: must NOT retry the open.
    bridge::process_event(
        &rig.deps,
        guild_msg(
            "2402",
            CHANNEL_ID,
            "<@999999999999999999> again",
            &[BOT_USER_ID],
        ),
    )
    .await
    .expect("second mention");
    assert_eq!(
        rig.opener.call_count(),
        1,
        "a non-threadable container is remembered (is_thread); no retry",
    );

    // Both still triggered, both replied into the same container.
    assert_eq!(
        count(&pool, "SELECT COUNT(*) FROM prompt_requests").await,
        2
    );
    let attached = rig.outbound.attached.lock().expect("lock");
    assert_eq!(attached.len(), 2);
    assert!(
        attached
            .iter()
            .all(|a| a.container_id.as_str() == CHANNEL_ID),
        "both replies land in the container",
    );
}

#[sqlx::test]
async fn re_mention_in_owned_thread_does_not_backfill(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let caller = Caller::new(seed.user_id, seed.org_id);
    let opener = Arc::new(FakeThreadOpener::returning(
        ContainerId::try_from(THREAD_ID).expect("thread id"),
    ));
    let history = Arc::new(CountingHistoryReader::default());
    let history_seam: SharedHistoryReader = history.clone();
    let rig = build_rig(&pool, &caller, seed.agent_id, opener, history_seam).await;

    // Open the thread (channel mention): backfill is skipped (thread != channel).
    bridge::process_event(
        &rig.deps,
        guild_msg(
            "2301",
            CHANNEL_ID,
            "<@999999999999999999> hi",
            &[BOT_USER_ID],
        ),
    )
    .await
    .expect("open thread");
    // A re-@mention inside the thread must NOT page history — a bot-opened thread
    // has no pre-thread history, so its binding is marked backfilled at open.
    bridge::process_event(
        &rig.deps,
        guild_msg(
            "2302",
            THREAD_ID,
            "<@999999999999999999> more",
            &[BOT_USER_ID],
        ),
    )
    .await
    .expect("re-mention in thread");

    assert_eq!(
        *history.calls.lock().expect("calls"),
        0,
        "a bot-opened thread never backfills (no pre-thread history)",
    );
}
