//! Discord bridge inbound-ingest tests (the "fake gateway feed").
//!
//! A decoded Gateway dispatch is handed straight to `bridge::process_event`
//! (bypassing the socket): it shadow-mints the sender, mirrors the channel +
//! thread, appends the `posted` row, and — on a DM or a bot @-mention — enqueues
//! a fresh trigger and attaches the outbound pump. These tests assert the
//! inbound side only (ingest + trigger enqueue), so no worker runs (a shadow
//! acting user would hang the pool on teardown) — the full reply path is covered
//! once the stream pump lands.

#![allow(clippy::expect_used)]

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
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
use patom::discord::thread_map::PgDiscordThreadStore;
use patom::discord::types::{ApplicationId, BotToken, DiscordUserId};
use patom::runtime::PgPromptQueue;
use patom::threads::PgThreadStore;

mod common;
use common::pg::seed_tenant;

const APP_ID: &str = "111111111111111111";
const BOT_USER_ID: &str = "999999999999999999";
const GUILD_ID: &str = "222222222222222222";
const CHANNEL_ID: &str = "333333333333333333";
const HUMAN_ID: &str = "444444444444444444";

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

struct Rig {
    deps: BridgeDeps,
    outbound: Arc<FakeOutboundAttach>,
}

async fn build_rig(pool: &PgPool, caller: &Caller, agent_id: patom::agents::AgentId) -> Rig {
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
    let deps = BridgeDeps {
        apps: app_store,
        directory: Arc::new(PgDiscordDirectory::new(pool.clone(), clock.clone())),
        channels: Arc::new(PgDiscordChannelStore::new(pool.clone(), clock.clone())),
        threads: Arc::new(PgDiscordThreadStore::new(pool.clone(), clock.clone())),
        thread_store: Arc::new(PgThreadStore::new(pool.clone(), clock.clone())),
        colleagues: Arc::new(PgColleagueStore::new(pool.clone())),
        queue: Arc::new(PgPromptQueue::new(pool.clone(), clock.clone())),
        outbound: outbound_seam,
    };
    Rig { deps, outbound }
}

/// Build a MESSAGE_CREATE dispatch. `mentions` are user snowflakes; `guild` is
/// `None` for a DM.
fn message_dispatch(
    msg_id: &str,
    content: &str,
    mentions: &[&str],
    guild: Option<&str>,
) -> InboundDispatch {
    let mut author =
        serde_json::json!({"id": HUMAN_ID, "username": "alice", "global_name": "Alice"});
    if guild.is_some() {
        author["bot"] = serde_json::Value::Bool(false);
    }
    let mut data = serde_json::json!({
        "id": msg_id,
        "channel_id": CHANNEL_ID,
        "author": author,
        "content": content,
        "mentions": mentions.iter().map(|m| serde_json::json!({"id": m})).collect::<Vec<_>>(),
    });
    if let Some(g) = guild {
        data["guild_id"] = serde_json::Value::String(g.to_owned());
    }
    InboundDispatch {
        application_id: ApplicationId::try_from(APP_ID).expect("app id"),
        bot_user_id: DiscordUserId::try_from(BOT_USER_ID).expect("bot id"),
        event_type: "MESSAGE_CREATE".to_owned(),
        data,
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
async fn ambient_guild_message_ingests_without_trigger(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let caller = Caller::new(seed.user_id, seed.org_id);
    let rig = build_rig(&pool, &caller, seed.agent_id).await;

    bridge::process_event(
        &rig.deps,
        message_dispatch("1001", "just chatting", &[], Some(GUILD_ID)),
    )
    .await
    .expect("process ambient");

    // The sender is shadow-minted (identity/context) and the channel + thread
    // are mirrored, with one posted row…
    assert_eq!(
        count(
            &pool,
            "SELECT COUNT(*) FROM discord_user_handles WHERE discord_user_id = '444444444444444444'"
        )
        .await,
        1,
        "ambient sender is shadow-minted",
    );
    assert_eq!(
        count(&pool, "SELECT COUNT(*) FROM discord_channels").await,
        1
    );
    assert_eq!(
        count(&pool, "SELECT COUNT(*) FROM discord_threads").await,
        1
    );
    assert_eq!(
        count(
            &pool,
            "SELECT COUNT(*) FROM thread_messages WHERE kind = 'posted'"
        )
        .await,
        1,
    );
    // …but NO trigger was enqueued and the pump was not attached.
    assert_eq!(
        count(&pool, "SELECT COUNT(*) FROM prompt_requests").await,
        0
    );
    assert!(rig.outbound.attached.lock().expect("lock").is_empty());
}

#[sqlx::test]
async fn mention_enqueues_a_trigger_and_attaches_pump(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let caller = Caller::new(seed.user_id, seed.org_id);
    let rig = build_rig(&pool, &caller, seed.agent_id).await;

    // The message @-mentions the bot → a trigger.
    bridge::process_event(
        &rig.deps,
        message_dispatch(
            "1002",
            "<@999999999999999999> draft a JD",
            &[BOT_USER_ID],
            Some(GUILD_ID),
        ),
    )
    .await
    .expect("process mention");

    assert_eq!(
        count(
            &pool,
            "SELECT COUNT(*) FROM thread_messages WHERE kind = 'posted'"
        )
        .await,
        1,
    );
    // Exactly one trigger, keyed on the Discord idempotency namespace.
    assert_eq!(
        count(&pool, "SELECT COUNT(*) FROM prompt_requests WHERE idempotency_key = 'discord:222222222222222222:1002'").await,
        1,
        "the mention enqueues a discord-namespaced trigger",
    );
    assert_eq!(
        rig.outbound.attached.lock().expect("lock").len(),
        1,
        "pump attached once"
    );
}

#[sqlx::test]
async fn dm_message_always_triggers(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let caller = Caller::new(seed.user_id, seed.org_id);
    let rig = build_rig(&pool, &caller, seed.agent_id).await;

    // A DM (no guild) is a trigger even without a mention.
    bridge::process_event(&rig.deps, message_dispatch("1003", "hello bot", &[], None))
        .await
        .expect("process dm");

    assert_eq!(
        count(&pool, "SELECT COUNT(*) FROM prompt_requests").await,
        1
    );
    assert_eq!(rig.outbound.attached.lock().expect("lock").len(), 1);
}

#[sqlx::test]
async fn bots_own_message_is_dropped(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let caller = Caller::new(seed.user_id, seed.org_id);
    let rig = build_rig(&pool, &caller, seed.agent_id).await;

    // A MESSAGE_CREATE whose author IS our bot → dropped before any DB work.
    let mut dispatch = message_dispatch("1004", "my own reply", &[], Some(GUILD_ID));
    dispatch.data["author"]["id"] = serde_json::Value::String(BOT_USER_ID.to_owned());
    bridge::process_event(&rig.deps, dispatch)
        .await
        .expect("process self");

    assert_eq!(
        count(
            &pool,
            "SELECT COUNT(*) FROM thread_messages WHERE kind = 'posted'"
        )
        .await,
        0
    );
    assert_eq!(
        count(&pool, "SELECT COUNT(*) FROM discord_user_handles").await,
        0
    );
    assert_eq!(
        count(&pool, "SELECT COUNT(*) FROM prompt_requests").await,
        0
    );
}

#[sqlx::test]
async fn redelivered_message_is_idempotent(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let caller = Caller::new(seed.user_id, seed.org_id);
    let rig = build_rig(&pool, &caller, seed.agent_id).await;

    let dispatch = message_dispatch("1005", "chatting", &[], Some(GUILD_ID));
    bridge::process_event(&rig.deps, dispatch.clone())
        .await
        .expect("first");
    // Same message_id redelivered (a resume replay / backfill overlap).
    bridge::process_event(&rig.deps, dispatch)
        .await
        .expect("second");

    // The idempotency_key absorbs the duplicate: one posted row, one shadow.
    assert_eq!(
        count(
            &pool,
            "SELECT COUNT(*) FROM thread_messages WHERE kind = 'posted'"
        )
        .await,
        1,
        "redelivery does not duplicate the mirrored row",
    );
    assert_eq!(
        count(&pool, "SELECT COUNT(*) FROM discord_user_handles").await,
        1
    );
}
