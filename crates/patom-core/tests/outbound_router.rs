//! Stage B (issue #178): the `OutboundRouter` seam + per-platform routers.
//!
//! Exercises arms 1 (already-bound) and 2 (channel thread) of the Discord and
//! Lark routers against real Postgres, with the surface pump stubbed by a
//! recording fake. Also covers the composite fan-out and the no-surface noop.

#![allow(clippy::expect_used)]

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use patom::auth::{Caller, OrgId};
use patom::clock::{SharedClock, SystemClock};
use patom::colleagues::{ColleagueId, resolve_agent_colleague, resolve_user_colleague};
use patom::crypto::OrgEncryptor;
use patom::outbound::{
    CompositeOutboundRouter, NoopOutboundRouter, OutboundError, OutboundRouter,
    SharedOutboundRouter,
};
use patom::threads::{PgThreadStore, ThreadId, ThreadStore};
use sqlx::PgPool;

use patom::discord::app_store::{DiscordAppStore, NewDiscordApp, PgDiscordAppStore};
use patom::discord::bridge::{
    AttachRequest as DiscordAttach, OutboundAttach, SharedOutboundAttach,
};
use patom::discord::channel_map::PgDiscordChannelStore;
use patom::discord::directory::{DiscordDirectory, PgDiscordDirectory};
use patom::discord::dm_map::{DiscordDmStore, PgDiscordDmStore};
use patom::discord::outbound_router::DiscordOutboundRouter;
use patom::discord::poster::FakeDiscordPoster;
use patom::discord::thread_map::{DiscordThreadStore, PgDiscordThreadStore};
use patom::discord::types::{ApplicationId, BotToken, ContainerId, DiscordUserId, GuildId};

use patom::lark::app_store::{LarkAppStore, NewLarkApp, PgLarkAppStore};
use patom::lark::channel_map::PgLarkChannelStore;
use patom::lark::directory::{LarkDirectory, PgLarkDirectory};
use patom::lark::dm_map::{LarkDmStore, PgLarkDmStore};
use patom::lark::outbound_router::{
    LarkOutboundAttach, LarkOutboundRouter, SharedLarkOutboundAttach,
};
use patom::lark::stream_pump::{AttachRequest as LarkAttach, LarkRecipient};
use patom::lark::thread_map::PgLarkThreadStore;
use patom::lark::types::{LarkAppId, LarkChatId, LarkOpenId, LarkUserId, TenantKey};

mod common;
use common::pg::seed_tenant;

const DISCORD_APP: &str = "111111111111111111";
const GUILD: &str = "222222222222222222";
const DISCORD_CHANNEL: &str = "333333333333333333";
const LARK_APP: &str = "cli_aaaaaaaaaaaa";
const LARK_TENANT: &str = "tk_test";
const LARK_CHAT: &str = "oc_abc123def456";

/// Records every attach a router requests of the (stubbed) Discord pump.
#[derive(Debug, Default)]
struct FakeDiscordAttach {
    attached: Mutex<Vec<DiscordAttach>>,
}
#[async_trait]
impl OutboundAttach for FakeDiscordAttach {
    async fn attach(&self, req: DiscordAttach) {
        self.attached.lock().expect("mutex").push(req);
    }
}

#[derive(Debug, Default)]
struct FakeLarkAttach {
    attached: Mutex<Vec<LarkAttach>>,
}
#[async_trait]
impl LarkOutboundAttach for FakeLarkAttach {
    async fn attach(&self, req: LarkAttach) {
        self.attached.lock().expect("mutex").push(req);
    }
}

fn discord_router(
    pool: &PgPool,
    clock: &SharedClock,
    pump: SharedOutboundAttach,
) -> DiscordOutboundRouter {
    discord_router_with_poster(pool, clock, pump, Arc::new(FakeDiscordPoster::new()))
}

fn discord_router_with_poster(
    pool: &PgPool,
    clock: &SharedClock,
    pump: SharedOutboundAttach,
    poster: Arc<FakeDiscordPoster>,
) -> DiscordOutboundRouter {
    DiscordOutboundRouter::new(
        Arc::new(PgDiscordThreadStore::new(pool.clone(), clock.clone())),
        Arc::new(PgDiscordChannelStore::new(pool.clone(), clock.clone())),
        Arc::new(PgDiscordAppStore::new(
            pool.clone(),
            clock.clone(),
            Arc::new(OrgEncryptor::for_test([7u8; 32])),
        )),
        Arc::new(PgDiscordDirectory::new(pool.clone(), clock.clone())),
        Arc::new(PgDiscordDmStore::new(pool.clone(), clock.clone())),
        poster,
        Arc::new(PgThreadStore::new(pool.clone(), clock.clone())),
        pump,
    )
}

async fn register_discord_app(
    pool: &PgPool,
    clock: &SharedClock,
    caller: &Caller,
    agent: patom::agents::AgentId,
) {
    let store = PgDiscordAppStore::new(
        pool.clone(),
        clock.clone(),
        Arc::new(OrgEncryptor::for_test([7u8; 32])),
    );
    store
        .register(
            caller,
            NewDiscordApp {
                application_id: ApplicationId::try_from(DISCORD_APP).expect("app id"),
                agent_id: agent,
                bot_token: BotToken::try_from("MTk4N.example.token".to_owned()).expect("token"),
            },
        )
        .await
        .expect("register discord app");
}

/// Create a channel thread in `channel_id` with `agent` participating, so
/// `channel_of` and `last_agent` both resolve.
async fn channel_thread(
    pool: &PgPool,
    caller: &Caller,
    org: OrgId,
    channel_id: patom::channels::ChannelId,
    agent: patom::agents::AgentId,
) -> ThreadId {
    let clock = SystemClock::shared();
    let store = PgThreadStore::new(pool.clone(), clock);
    let creator = resolve_agent_colleague(pool, org, agent)
        .await
        .expect("agent colleague");
    let thread = store
        .create_thread(caller, Some(channel_id), None, creator, None)
        .await
        .expect("create thread");
    store
        .resolve_participation(caller, thread, agent)
        .await
        .expect("participation");
    thread
}

#[sqlx::test]
async fn discord_arm2_channel_thread_attaches(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let caller = Caller::new(seed.user_id, seed.org_id);
    let clock = SystemClock::shared();
    register_discord_app(&pool, &clock, &caller, seed.agent_id).await;

    let channels = PgDiscordChannelStore::new(pool.clone(), clock.clone());
    let guild = GuildId::try_from(GUILD).expect("guild");
    let container = ContainerId::try_from(DISCORD_CHANNEL).expect("container");
    let channel_id = {
        use patom::discord::channel_map::DiscordChannelStore;
        channels
            .ensure_channel(seed.org_id, &guild, &container, seed.user_id)
            .await
            .expect("ensure_channel")
    };
    let thread = channel_thread(&pool, &caller, seed.org_id, channel_id, seed.agent_id).await;

    let pump = Arc::new(FakeDiscordAttach::default());
    let router = discord_router(&pool, &clock, pump.clone());
    router
        .ensure_delivery(seed.org_id, thread)
        .await
        .expect("ensure_delivery");

    let attached = pump.attached.lock().expect("mutex");
    assert_eq!(
        attached.len(),
        1,
        "channel thread attaches exactly one pump"
    );
    assert_eq!(attached[0].thread_id, thread);
    assert_eq!(attached[0].container_id, container);
    assert_eq!(
        attached[0].application_id,
        ApplicationId::try_from(DISCORD_APP).expect("app id"),
    );
}

#[sqlx::test]
async fn discord_arm1_bound_thread_attaches(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let caller = Caller::new(seed.user_id, seed.org_id);
    let clock = SystemClock::shared();
    register_discord_app(&pool, &clock, &caller, seed.agent_id).await;

    // A DM-less / channel-less thread that we bind directly in discord_threads.
    let store = PgThreadStore::new(pool.clone(), clock.clone());
    let creator = resolve_agent_colleague(&pool, seed.org_id, seed.agent_id)
        .await
        .expect("agent colleague");
    let human = common::pg::human_participant(&pool, seed.org_id, seed.user_id).await;
    let counterpart = match human {
        patom::types::Participant::Human { colleague_id, .. } => colleague_id,
        _ => unreachable!("human"),
    };
    let thread = store
        .create_thread(&caller, None, None, creator, Some(counterpart))
        .await
        .expect("create dm thread");

    let threads_map = PgDiscordThreadStore::new(pool.clone(), clock.clone());
    let guild = GuildId::try_from(GUILD).expect("guild");
    let container = ContainerId::try_from(DISCORD_CHANNEL).expect("container");
    threads_map
        .bind(
            seed.org_id,
            &ApplicationId::try_from(DISCORD_APP).expect("app id"),
            &guild,
            &container,
            None,
            false,
            thread,
        )
        .await
        .expect("bind");

    let pump = Arc::new(FakeDiscordAttach::default());
    let router = discord_router(&pool, &clock, pump.clone());
    router
        .ensure_delivery(seed.org_id, thread)
        .await
        .expect("ensure_delivery");

    let attached = pump.attached.lock().expect("mutex");
    assert_eq!(attached.len(), 1, "bound thread attaches via arm 1");
    assert_eq!(attached[0].container_id, container);
}

#[sqlx::test]
async fn discord_router_ignores_lark_channel_thread(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let caller = Caller::new(seed.user_id, seed.org_id);
    let clock = SystemClock::shared();
    register_discord_app(&pool, &clock, &caller, seed.agent_id).await;

    // A channel that maps to LARK, not Discord.
    let lark_channels = PgLarkChannelStore::new(pool.clone(), clock.clone());
    let channel_id = {
        use patom::lark::channel_map::LarkChannelStore;
        lark_channels
            .ensure_channel(
                seed.org_id,
                &TenantKey::try_from(LARK_TENANT).expect("tenant"),
                &LarkChatId::try_from(LARK_CHAT).expect("chat"),
                seed.user_id,
            )
            .await
            .expect("ensure_channel")
    };
    let thread = channel_thread(&pool, &caller, seed.org_id, channel_id, seed.agent_id).await;

    let pump = Arc::new(FakeDiscordAttach::default());
    let router = discord_router(&pool, &clock, pump.clone());
    router
        .ensure_delivery(seed.org_id, thread)
        .await
        .expect("ensure_delivery");

    assert!(
        pump.attached.lock().expect("mutex").is_empty(),
        "a Lark-backed channel is not Discord's surface"
    );
}

#[sqlx::test]
async fn lark_arm2_channel_thread_attaches(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let caller = Caller::new(seed.user_id, seed.org_id);
    let clock = SystemClock::shared();

    // Register a Lark app so app_id_for_agent resolves.
    let apps = PgLarkAppStore::new(
        pool.clone(),
        clock.clone(),
        Arc::new(OrgEncryptor::for_test([7u8; 32])),
    );
    apps.register(
        &caller,
        NewLarkApp {
            app_id: LarkAppId::try_from(LARK_APP).expect("app id"),
            agent_id: seed.agent_id,
            app_secret: patom::lark::types::LarkAppSecret::try_from("secret-value".to_owned())
                .expect("secret"),
        },
    )
    .await
    .expect("register lark app");

    let lark_channels = PgLarkChannelStore::new(pool.clone(), clock.clone());
    let chat = LarkChatId::try_from(LARK_CHAT).expect("chat");
    let channel_id = {
        use patom::lark::channel_map::LarkChannelStore;
        lark_channels
            .ensure_channel(
                seed.org_id,
                &TenantKey::try_from(LARK_TENANT).expect("tenant"),
                &chat,
                seed.user_id,
            )
            .await
            .expect("ensure_channel")
    };
    let thread = channel_thread(&pool, &caller, seed.org_id, channel_id, seed.agent_id).await;

    let pump = Arc::new(FakeLarkAttach::default());
    let pump_seam: SharedLarkOutboundAttach = pump.clone();
    let router = LarkOutboundRouter::new(
        Arc::new(PgLarkThreadStore::new(pool.clone(), clock.clone())),
        Arc::new(lark_channels),
        Arc::new(apps),
        Arc::new(PgLarkDirectory::new(pool.clone(), clock.clone())),
        Arc::new(PgLarkDmStore::new(pool.clone(), clock.clone())),
        Arc::new(PgThreadStore::new(pool.clone(), clock.clone())),
        pump_seam,
    );
    router
        .ensure_delivery(seed.org_id, thread)
        .await
        .expect("ensure_delivery");

    let attached = pump.attached.lock().expect("mutex");
    assert_eq!(attached.len(), 1, "lark channel thread attaches one pump");
    match &attached[0].recipient {
        LarkRecipient::Chat { chat_id, .. } => assert_eq!(*chat_id, chat),
        LarkRecipient::Dm { .. } => panic!("channel thread must attach a Chat recipient"),
    }
}

/// Create a DM thread between `agent` and `counterpart`, with `agent`
/// participating (so `dm_counterpart` and `last_agent` both resolve).
async fn dm_thread(
    pool: &PgPool,
    caller: &Caller,
    org: OrgId,
    counterpart: ColleagueId,
    agent: patom::agents::AgentId,
) -> ThreadId {
    let clock = SystemClock::shared();
    let store = PgThreadStore::new(pool.clone(), clock);
    let creator = resolve_agent_colleague(pool, org, agent)
        .await
        .expect("agent colleague");
    let thread = store
        .create_thread(caller, None, None, creator, Some(counterpart))
        .await
        .expect("create dm thread");
    store
        .resolve_participation(caller, thread, agent)
        .await
        .expect("participation");
    thread
}

#[sqlx::test]
async fn discord_arm3_dm_opens_binds_and_attaches(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let caller = Caller::new(seed.user_id, seed.org_id);
    let clock = SystemClock::shared();
    register_discord_app(&pool, &clock, &caller, seed.agent_id).await;

    let recipient = DiscordUserId::try_from("444444444444444444").expect("snowflake");
    let dir = PgDiscordDirectory::new(pool.clone(), clock.clone());
    let shadow = dir
        .resolve_or_mint(seed.org_id, &recipient, Some("Alice"))
        .await
        .expect("mint shadow");
    let thread = dm_thread(
        &pool,
        &caller,
        seed.org_id,
        shadow.colleague_id,
        seed.agent_id,
    )
    .await;

    let pump = Arc::new(FakeDiscordAttach::default());
    let poster = Arc::new(FakeDiscordPoster::new());
    let router = discord_router_with_poster(&pool, &clock, pump.clone(), poster.clone());
    router
        .ensure_delivery(seed.org_id, thread)
        .await
        .expect("ensure_delivery");

    let opens = poster.dm_opens();
    assert_eq!(opens.len(), 1, "opens the DM channel once");
    assert_eq!(opens[0].1, recipient, "opens with the recipient snowflake");
    {
        let attached = pump.attached.lock().expect("mutex");
        assert_eq!(attached.len(), 1, "attaches the pump to the DM channel");
        assert_eq!(
            attached[0].container_id,
            ContainerId::try_from("900000000000000001").expect("fake dm channel"),
        );
    }
    let dms = PgDiscordDmStore::new(pool.clone(), clock.clone());
    assert!(
        dms.lookup_by_patom_thread(thread)
            .await
            .expect("lookup")
            .is_some(),
        "the DM is bound",
    );

    // Re-fire: arm 1b reuses the binding — no second create_dm.
    router
        .ensure_delivery(seed.org_id, thread)
        .await
        .expect("ensure_delivery again");
    assert_eq!(
        poster.dm_opens().len(),
        1,
        "a re-fire reuses the DM binding rather than opening a second channel"
    );
}

#[sqlx::test]
async fn discord_dm_noop_when_counterpart_not_a_shadow(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let caller = Caller::new(seed.user_id, seed.org_id);
    let clock = SystemClock::shared();
    register_discord_app(&pool, &clock, &caller, seed.agent_id).await;

    // The seeded human has no Discord shadow handle.
    let human = resolve_user_colleague(&pool, seed.org_id, seed.user_id)
        .await
        .expect("human colleague");
    let thread = dm_thread(&pool, &caller, seed.org_id, human, seed.agent_id).await;

    let pump = Arc::new(FakeDiscordAttach::default());
    let poster = Arc::new(FakeDiscordPoster::new());
    let router = discord_router_with_poster(&pool, &clock, pump.clone(), poster.clone());
    router
        .ensure_delivery(seed.org_id, thread)
        .await
        .expect("ensure_delivery");

    assert!(
        poster.dm_opens().is_empty(),
        "no DM opened for a non-shadow"
    );
    assert!(
        pump.attached.lock().expect("mutex").is_empty(),
        "a counterpart with no Discord shadow stays web-only"
    );
}

#[sqlx::test]
async fn lark_arm3_dm_binds_and_attaches(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let caller = Caller::new(seed.user_id, seed.org_id);
    let clock = SystemClock::shared();

    let apps = PgLarkAppStore::new(
        pool.clone(),
        clock.clone(),
        Arc::new(OrgEncryptor::for_test([7u8; 32])),
    );
    apps.register(
        &caller,
        NewLarkApp {
            app_id: LarkAppId::try_from(LARK_APP).expect("app id"),
            agent_id: seed.agent_id,
            app_secret: patom::lark::types::LarkAppSecret::try_from("secret-value".to_owned())
                .expect("secret"),
        },
    )
    .await
    .expect("register lark app");

    let dir = PgLarkDirectory::new(pool.clone(), clock.clone());
    let open_id = LarkOpenId::try_from("ou_recipient123").expect("open id");
    let shadow = dir
        .resolve_or_mint(
            seed.org_id,
            &TenantKey::try_from(LARK_TENANT).expect("tenant"),
            &LarkUserId::try_from("lu_alice123").expect("user id"),
            &open_id,
            Some("Alice"),
        )
        .await
        .expect("mint shadow");
    let thread = dm_thread(
        &pool,
        &caller,
        seed.org_id,
        shadow.colleague_id,
        seed.agent_id,
    )
    .await;

    let pump = Arc::new(FakeLarkAttach::default());
    let pump_seam: SharedLarkOutboundAttach = pump.clone();
    let dms = Arc::new(PgLarkDmStore::new(pool.clone(), clock.clone()));
    let router = LarkOutboundRouter::new(
        Arc::new(PgLarkThreadStore::new(pool.clone(), clock.clone())),
        Arc::new(PgLarkChannelStore::new(pool.clone(), clock.clone())),
        Arc::new(apps),
        Arc::new(PgLarkDirectory::new(pool.clone(), clock.clone())),
        dms.clone(),
        Arc::new(PgThreadStore::new(pool.clone(), clock.clone())),
        pump_seam,
    );
    router
        .ensure_delivery(seed.org_id, thread)
        .await
        .expect("ensure_delivery");

    {
        let attached = pump.attached.lock().expect("mutex");
        assert_eq!(attached.len(), 1, "lark DM attaches one pump");
        match &attached[0].recipient {
            LarkRecipient::Dm { open_id: got } => assert_eq!(*got, open_id),
            LarkRecipient::Chat { .. } => panic!("a DM thread must attach a Dm recipient"),
        }
    }
    assert!(
        dms.lookup_by_patom_thread(thread)
            .await
            .expect("lookup")
            .is_some(),
        "the Lark DM is bound",
    );
}

// --- composite + noop -------------------------------------------------------

#[derive(Debug, Default)]
struct RecordingRouter {
    calls: Mutex<usize>,
}
#[async_trait]
impl OutboundRouter for RecordingRouter {
    async fn ensure_delivery(&self, _org: OrgId, _thread: ThreadId) -> Result<(), OutboundError> {
        *self.calls.lock().expect("mutex") += 1;
        Ok(())
    }
}

#[derive(Debug)]
struct FailingRouter;
#[async_trait]
impl OutboundRouter for FailingRouter {
    async fn ensure_delivery(&self, _org: OrgId, _thread: ThreadId) -> Result<(), OutboundError> {
        Err(OutboundError::Backend("boom".to_owned()))
    }
}

#[tokio::test]
async fn composite_fans_out_and_survives_a_failing_router() {
    let recording = Arc::new(RecordingRouter::default());
    let routers: Vec<SharedOutboundRouter> = vec![Arc::new(FailingRouter), recording.clone()];
    let composite = CompositeOutboundRouter::new(routers);

    composite
        .ensure_delivery(OrgId::new(), ThreadId::new())
        .await
        .expect("composite swallows per-router errors");

    assert_eq!(
        *recording.calls.lock().expect("mutex"),
        1,
        "a failing router does not abort the others"
    );
}

#[tokio::test]
async fn noop_router_is_a_noop() {
    NoopOutboundRouter
        .ensure_delivery(OrgId::new(), ThreadId::new())
        .await
        .expect("noop");
}
