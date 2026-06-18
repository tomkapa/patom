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

use patom::approvals::{
    ActionSummary, ApprovalDecider, ApprovalId, ApprovalResumer, ApprovalStore, ApproverPolicy,
    NewApproval, PgApprovalStore, PlatformTarget, SharedApprovalDecider,
};
use patom::assets::InMemoryAssetStore;
use patom::auth::{Caller, run_privileged};
use patom::clock::{SharedClock, SystemClock};
use patom::colleagues::PgColleagueStore;
use patom::colleagues::{resolve_agent_colleague, resolve_user_colleague};
use patom::crypto::OrgEncryptor;
use patom::discord::app_store::{DiscordAppStore, NewDiscordApp, PgDiscordAppStore};
use patom::discord::attachment::{FakeAttachmentFetcher, SharedAttachmentFetcher};
use patom::discord::bridge::{
    self, AttachRequest, BridgeDeps, OutboundAttach, SharedOutboundAttach,
};
use patom::discord::channel_map::PgDiscordChannelStore;
use patom::discord::connection::InboundDispatch;
use patom::discord::directory::PgDiscordDirectory;
use patom::discord::history::FakeHistoryReader;
use patom::discord::poster::FakeDiscordPoster;
use patom::discord::thread_map::PgDiscordThreadStore;
use patom::discord::thread_opener::{FakeThreadOpener, SharedThreadOpener};
use patom::discord::types::{ApplicationId, BotToken, ContainerId, DiscordUserId};
use patom::runtime::{PgDagBudget, PgPromptQueue};
use patom::threads::{PgThreadStore, ThreadId};
use patom::types::ToolName;

mod common;
use chrono::{Duration, Utc};
use common::pg::{Seed, seed_agent_thread_state, seed_prompt_request, seed_tenant};
use uuid::Uuid;

const APP_ID: &str = "111111111111111111";
const BOT_USER_ID: &str = "999999999999999999";
const GUILD_ID: &str = "222222222222222222";
const CHANNEL_ID: &str = "333333333333333333";
const HUMAN_ID: &str = "444444444444444444";
/// The thread id the rig's `FakeThreadOpener` returns when the bridge opens one.
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

struct Rig {
    deps: BridgeDeps,
    outbound: Arc<FakeOutboundAttach>,
    opener: Arc<FakeThreadOpener>,
    /// In-memory asset store backing the bridge's attachment re-hosting.
    assets: Arc<InMemoryAssetStore>,
    /// Canned-bytes fetcher; populate per-URL before driving an attachment test.
    fetcher: Arc<FakeAttachmentFetcher>,
    /// Records the interaction ack / resolved-card edit / ephemeral rejection so
    /// the approval-button tests can assert what the clicker saw.
    poster: Arc<FakeDiscordPoster>,
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
    let opener = Arc::new(FakeThreadOpener::returning(
        ContainerId::try_from(THREAD_ID).expect("thread id"),
    ));
    let thread_opener: SharedThreadOpener = opener.clone();
    let assets = Arc::new(InMemoryAssetStore::new("https://asset.example"));
    let fetcher = Arc::new(FakeAttachmentFetcher::new());
    let attachment_fetcher: SharedAttachmentFetcher = fetcher.clone();
    let poster = Arc::new(FakeDiscordPoster::new());
    // The shared approval seam over the real store — the interaction tests drive
    // it end-to-end (authorize → decide → resume).
    let decider: SharedApprovalDecider = Arc::new(ApprovalDecider::new(
        Arc::new(PgApprovalStore::new(pool.clone(), clock.clone())),
        Arc::new(ApprovalResumer::new(
            Arc::new(PgThreadStore::new(pool.clone(), clock.clone())),
            Arc::new(PgPromptQueue::new(pool.clone(), clock.clone())),
            Arc::new(PgDagBudget::new(pool.clone())),
            Arc::new(PgColleagueStore::new(pool.clone())),
        )),
        clock.clone(),
    ));
    let deps = BridgeDeps {
        apps: app_store,
        directory: Arc::new(PgDiscordDirectory::new(pool.clone(), clock.clone())),
        channels: Arc::new(PgDiscordChannelStore::new(pool.clone(), clock.clone())),
        threads: Arc::new(PgDiscordThreadStore::new(pool.clone(), clock.clone())),
        thread_store: Arc::new(PgThreadStore::new(pool.clone(), clock.clone())),
        colleagues: Arc::new(PgColleagueStore::new(pool.clone())),
        queue: Arc::new(PgPromptQueue::new(pool.clone(), clock.clone())),
        outbound: outbound_seam,
        // No backfill in these tests (empty reader) — the live ingest path is
        // what's under test here.
        history: Arc::new(FakeHistoryReader::empty()),
        thread_opener,
        assets: Some(assets.clone()),
        attachment_fetcher,
        decider: Some(decider),
        poster: Some(poster.clone()),
    };
    Rig {
        deps,
        outbound,
        opener,
        assets,
        fetcher,
        poster,
    }
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
    // …but NO trigger was enqueued, the pump was not attached, and an ambient
    // message never opens a thread.
    assert_eq!(
        count(&pool, "SELECT COUNT(*) FROM prompt_requests").await,
        0
    );
    assert!(rig.outbound.attached.lock().expect("lock").is_empty());
    assert_eq!(rig.opener.call_count(), 0, "ambient ingest opens no thread");
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
    // Exactly one trigger, keyed on the Discord idempotency namespace
    // (app-scoped: `discord:{app}:{guild}:{message}`).
    assert_eq!(
        count(&pool, "SELECT COUNT(*) FROM prompt_requests WHERE idempotency_key = 'discord:111111111111111111:222222222222222222:1002'").await,
        1,
        "the mention enqueues a discord-namespaced trigger",
    );
    // The top-level @mention OPENS a thread and converses there (not the channel).
    assert_eq!(rig.opener.call_count(), 1, "a thread was opened once");
    assert_eq!(
        count(
            &pool,
            "SELECT COUNT(*) FROM discord_threads \
             WHERE container_id = '555000000000000001' AND parent_id = '333333333333333333'"
        )
        .await,
        1,
        "the bound thread records its parent channel (is_thread)",
    );
    let attached = rig.outbound.attached.lock().expect("lock");
    assert_eq!(attached.len(), 1, "pump attached once");
    assert_eq!(
        attached[0].container_id.as_str(),
        THREAD_ID,
        "the reply routes to the opened thread, not the channel",
    );
    assert!(
        attached[0].reply_to.is_none(),
        "no inline message_reference inside a fresh thread",
    );
}

/// Minimal valid PNG prefix padded out — enough to pass the magic-byte sniff.
fn png_bytes() -> Vec<u8> {
    let mut v = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    v.resize(128, 0);
    v
}

async fn one_posted_body(pool: &PgPool) -> String {
    run_privileged::<String, sqlx::Error>(pool, async |tx| {
        sqlx::query_scalar("SELECT body::text FROM thread_messages WHERE kind = 'posted' LIMIT 1")
            .fetch_one(&mut **tx)
            .await
    })
    .await
    .expect("body")
}

#[sqlx::test]
async fn mention_with_image_attachment_rehosts_as_model_input(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let caller = Caller::new(seed.user_id, seed.org_id);
    let rig = build_rig(&pool, &caller, seed.agent_id).await;

    let cdn = "https://cdn.discordapp.com/attachments/333/77/pic.png?ex=a&is=b&hm=c";
    rig.fetcher.insert(cdn, png_bytes(), Some("image/png"));

    let mut dispatch = message_dispatch(
        "2001",
        "<@999999999999999999> what is this?",
        &[BOT_USER_ID],
        Some(GUILD_ID),
    );
    dispatch.data["attachments"] = serde_json::json!([
        {"id": "77", "filename": "pic.png", "size": 128, "content_type": "image/png", "url": cdn}
    ]);

    bridge::process_event(&rig.deps, dispatch)
        .await
        .expect("process attachment mention");

    // The image was downloaded and re-hosted exactly once…
    assert_eq!(rig.assets.len().await, 1, "one object stored");
    // …and the mirrored message body carries an image block pointing at the
    // re-hosted asset URL (not the ephemeral Discord CDN link).
    let body = one_posted_body(&pool).await;
    assert!(
        body.contains("\"image\""),
        "body has an image block: {body}"
    );
    assert!(
        body.contains("https://asset.example/attachments/"),
        "image points at the re-hosted asset: {body}",
    );
    assert!(
        !body.contains("cdn.discordapp.com"),
        "the ephemeral CDN url is not persisted: {body}",
    );
}

#[sqlx::test]
async fn unsupported_attachment_is_skipped_but_message_mirrors(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let caller = Caller::new(seed.user_id, seed.org_id);
    let rig = build_rig(&pool, &caller, seed.agent_id).await;

    // A video attachment: unsupported model input → skipped, never downloaded.
    let cdn = "https://cdn.discordapp.com/attachments/333/88/clip.mp4?ex=a&is=b&hm=c";
    let mut dispatch = message_dispatch("2002", "see clip", &[], Some(GUILD_ID));
    dispatch.data["attachments"] = serde_json::json!([
        {"id": "88", "filename": "clip.mp4", "size": 4096, "content_type": "video/mp4", "url": cdn}
    ]);

    bridge::process_event(&rig.deps, dispatch)
        .await
        .expect("process unsupported attachment");

    // Nothing stored, but the text still mirrored as one posted row.
    assert!(
        rig.assets.is_empty().await,
        "unsupported type stores nothing"
    );
    assert_eq!(
        count(
            &pool,
            "SELECT COUNT(*) FROM thread_messages WHERE kind = 'posted'"
        )
        .await,
        1,
    );
    let body = one_posted_body(&pool).await;
    assert!(body.contains("see clip"), "text still mirrored: {body}");
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
    // A DM never opens a thread — it replies inline in the DM channel.
    assert_eq!(rig.opener.call_count(), 0, "no thread opened for a DM");
    let attached = rig.outbound.attached.lock().expect("lock");
    assert_eq!(attached.len(), 1);
    assert_eq!(
        attached[0].container_id.as_str(),
        CHANNEL_ID,
        "the reply lands in the DM channel",
    );
    assert!(
        attached[0].reply_to.is_some(),
        "a DM reply threads under the triggering message",
    );
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

const APP_ID_B: &str = "111111111111111112";
const BOT_USER_ID_B: &str = "999999999999999998";

#[sqlx::test]
async fn two_bots_in_one_guild_do_not_dedupe_each_others_mirror(pool: PgPool) {
    // Regression: the mirror idempotency key must be app-scoped. Two bots share a
    // guild, so the SAME Discord message reaches BOTH connections. With an
    // app-blind `guild:message` key the ambient bot's row deduped the triggered
    // bot's append (dedup is org-global on `(org_id, idempotency_key)`), leaving
    // the triggered bot's freshly opened thread empty — which then tripped the
    // non-empty-feed assertion in the agent turn (turn.rs).
    let seed = seed_tenant(&pool).await;
    let caller = Caller::new(seed.user_id, seed.org_id);
    let rig = build_rig(&pool, &caller, seed.agent_id).await;
    // A SECOND bot (app B) in the same org, speaking as a DISTINCT agent
    // (discord_apps enforces one app per agent via UNIQUE (org_id, agent_id)).
    let agent_b = common::pg::seed_agent(&pool, seed.org_id, "marketing-lead").await;
    rig.deps
        .apps
        .register(
            &caller,
            NewDiscordApp {
                application_id: ApplicationId::try_from(APP_ID_B).expect("app b"),
                agent_id: agent_b,
                bot_token: BotToken::try_from("MTk4N.example.tokenb".to_owned()).expect("token b"),
            },
        )
        .await
        .expect("register app b");

    // One human message @-mentioning bot B reaches both connections.
    let content = "<@999999999999999998> draft a JD";
    // Bot A sees it but isn't mentioned → ambient ingest (mirrors into A's thread).
    bridge::process_event(
        &rig.deps,
        message_dispatch("2001", content, &[BOT_USER_ID_B], Some(GUILD_ID)),
    )
    .await
    .expect("process via bot A");
    // Bot B sees the SAME message and IS mentioned → trigger (opens a thread).
    let mut dispatch_b = message_dispatch("2001", content, &[BOT_USER_ID_B], Some(GUILD_ID));
    dispatch_b.application_id = ApplicationId::try_from(APP_ID_B).expect("app b id");
    dispatch_b.bot_user_id = DiscordUserId::try_from(BOT_USER_ID_B).expect("bot b id");
    bridge::process_event(&rig.deps, dispatch_b)
        .await
        .expect("process via bot B");

    // Both bots mirror the message into their OWN thread — two posted rows, not
    // one. Pre-fix this was 1 (B's append deduped against A's row).
    assert_eq!(
        count(
            &pool,
            "SELECT COUNT(*) FROM thread_messages WHERE kind = 'posted'"
        )
        .await,
        2,
        "each bot's mirror survives — no cross-bot dedupe collision",
    );
    // The triggered bot's opened thread is non-empty — the exact condition the
    // agent turn asserts before building its request.
    assert_eq!(
        count(
            &pool,
            "SELECT COUNT(*) FROM thread_messages m \
             JOIN discord_threads dt ON dt.patom_thread_id = m.thread_id \
             WHERE dt.container_id = '555000000000000001' AND m.kind = 'posted'"
        )
        .await,
        1,
        "the triggered bot's freshly opened thread received the mirrored message",
    );
    // Bot B enqueued exactly one trigger.
    assert_eq!(
        count(&pool, "SELECT COUNT(*) FROM prompt_requests").await,
        1,
    );
}

#[sqlx::test]
async fn two_bots_bind_distinct_threads_for_same_container(pool: PgPool) {
    // Regression (multi-bot/-org scoping): two bots that both ingest an AMBIENT
    // message in the SAME channel must each bind their OWN Patom thread. A
    // binding keyed only by (guild, container) lets the first bot's row shadow
    // the second's (ON CONFLICT DO NOTHING), routing B's events into A's thread.
    let seed = seed_tenant(&pool).await;
    let caller = Caller::new(seed.user_id, seed.org_id);
    let rig = build_rig(&pool, &caller, seed.agent_id).await;
    let agent_b = common::pg::seed_agent(&pool, seed.org_id, "marketing-lead").await;
    rig.deps
        .apps
        .register(
            &caller,
            NewDiscordApp {
                application_id: ApplicationId::try_from(APP_ID_B).expect("app b"),
                agent_id: agent_b,
                bot_token: BotToken::try_from("MTk4N.example.tokenb".to_owned()).expect("token b"),
            },
        )
        .await
        .expect("register app b");

    // An ambient message (no mention) in the shared channel, delivered to both
    // bots' connections.
    bridge::process_event(
        &rig.deps,
        message_dispatch("3001", "hello team", &[], Some(GUILD_ID)),
    )
    .await
    .expect("via bot A");
    let mut dispatch_b = message_dispatch("3001", "hello team", &[], Some(GUILD_ID));
    dispatch_b.application_id = ApplicationId::try_from(APP_ID_B).expect("app b id");
    dispatch_b.bot_user_id = DiscordUserId::try_from(BOT_USER_ID_B).expect("bot b id");
    bridge::process_event(&rig.deps, dispatch_b)
        .await
        .expect("via bot B");

    // Two distinct bindings for the SAME container — one per bot — pointing at
    // distinct Patom threads. Pre-fix this was a single shared row.
    assert_eq!(
        count(
            &pool,
            "SELECT COUNT(DISTINCT patom_thread_id) FROM discord_threads \
             WHERE container_id = '333333333333333333'"
        )
        .await,
        2,
        "each bot binds its own Patom thread for the shared channel",
    );
}

// ─────────────────────────────────────────────────────────────────────────
// Approval-button intake (INTERACTION_CREATE) — issue #214
// ─────────────────────────────────────────────────────────────────────────

const INTERACTION_ID: &str = "700000000000000001";
/// A Discord user who clicks a button — distinct from the seeded tenant user, so
/// they shadow-mint to their own colleague.
const CLICKER_ID: &str = "666666666666666666";

/// Seed a pending approval row (own thread + DAG root) and return its id. The
/// `approvers` policy gates who may decide it.
async fn seed_approval(
    pool: &PgPool,
    seed: &Seed,
    approvers: ApproverPolicy,
    key: &str,
) -> ApprovalId {
    let clock: SharedClock = SystemClock::shared();
    let state = seed_agent_thread_state(pool, seed.org_id, seed.agent_id).await;
    let root = seed_prompt_request(pool, state, seed.agent_id, seed.org_id, seed.user_id).await;
    let (thread_id,): (Uuid,) =
        sqlx::query_as("SELECT thread_id FROM agent_thread_state WHERE id = $1")
            .bind(state)
            .fetch_one(pool)
            .await
            .expect("thread id for state");
    let agent_colleague = resolve_agent_colleague(pool, seed.org_id, seed.agent_id)
        .await
        .expect("agent colleague");
    let store = PgApprovalStore::new(pool.clone(), clock);
    let caller = Caller::new(seed.user_id, seed.org_id);
    let created = store
        .create(
            &caller,
            NewApproval {
                id: ApprovalId::new(),
                thread_id: ThreadId::from(thread_id),
                requesting_agent_id: seed.agent_id,
                requesting_colleague_id: agent_colleague,
                root_request_id: root,
                action_summary: ActionSummary::try_from("Refund $40 to customer #12")
                    .expect("summary"),
                gated_tool: ToolName::try_from("refund_customer").expect("tool"),
                approvers,
                target: PlatformTarget::Discord {
                    application_id: APP_ID.to_owned(),
                    container_id: CHANNEL_ID.to_owned(),
                    reply_to: None,
                },
                idempotency_key: key.to_owned(),
                expires_at: Utc::now() + Duration::hours(1),
            },
        )
        .await
        .expect("create approval");
    created.record().id
}

/// Build an INTERACTION_CREATE dispatch for a button click (`custom_id`) by
/// `clicker` (a Discord user snowflake).
fn interaction_dispatch(custom_id: &str, clicker: &str) -> InboundDispatch {
    let data = serde_json::json!({
        "id": INTERACTION_ID,
        "application_id": APP_ID,
        "token": "aW50ZXJhY3Rpb24tdG9rZW4",
        "type": 3,
        "channel_id": CHANNEL_ID,
        "member": { "user": {"id": clicker, "username": "clicker", "global_name": "Clicker"} },
        "data": { "custom_id": custom_id, "component_type": 2 },
    });
    InboundDispatch {
        application_id: ApplicationId::try_from(APP_ID).expect("app id"),
        bot_user_id: DiscordUserId::try_from(BOT_USER_ID).expect("bot id"),
        event_type: "INTERACTION_CREATE".to_owned(),
        data,
    }
}

/// The stored status of an approval row, read privileged.
async fn approval_status(pool: &PgPool, id: ApprovalId) -> String {
    run_privileged::<String, sqlx::Error>(pool, async move |tx| {
        sqlx::query_scalar("SELECT status FROM pending_approval WHERE id = $1")
            .bind(id.as_uuid())
            .fetch_one(&mut **tx)
            .await
    })
    .await
    .expect("status")
}

#[sqlx::test]
async fn approval_button_approve_decides_and_edits_card(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let caller = Caller::new(seed.user_id, seed.org_id);
    let rig = build_rig(&pool, &caller, seed.agent_id).await;
    let id = seed_approval(&pool, &seed, ApproverPolicy::Anyone, "apv:i1").await;

    bridge::process_event(
        &rig.deps,
        interaction_dispatch(&format!("apv:{}:a", id.as_uuid()), CLICKER_ID),
    )
    .await
    .expect("process interaction");

    // Acked once (within the 3s deadline), the card edited to the resolved view,
    // and no ephemeral rejection.
    let acks = rig.poster.acks();
    assert_eq!(acks.len(), 1, "interaction acked exactly once");
    assert_eq!(acks[0].as_str(), INTERACTION_ID);
    let edits = rig.poster.edits();
    assert_eq!(edits.len(), 1, "card edited to resolved view");
    assert!(
        edits[0].1.contains("Approved"),
        "resolved card shows the approval: {}",
        edits[0].1
    );
    assert!(rig.poster.followups().is_empty(), "no ephemeral rejection");
    assert_eq!(approval_status(&pool, id).await, "approved");
}

#[sqlx::test]
async fn unauthorized_click_is_rejected_without_transition(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let caller = Caller::new(seed.user_id, seed.org_id);
    let rig = build_rig(&pool, &caller, seed.agent_id).await;
    // Whitelist only the seeded human; the Discord clicker mints a different
    // colleague and so cannot decide.
    let human = resolve_user_colleague(&pool, seed.org_id, seed.user_id)
        .await
        .expect("human colleague");
    let id = seed_approval(&pool, &seed, ApproverPolicy::OneOf(vec![human]), "apv:i2").await;

    bridge::process_event(
        &rig.deps,
        interaction_dispatch(&format!("apv:{}:a", id.as_uuid()), CLICKER_ID),
    )
    .await
    .expect("process interaction");

    // Acked, but rejected ephemerally; the shared card is untouched and the row
    // stays pending.
    assert_eq!(rig.poster.acks().len(), 1, "still acked");
    assert_eq!(
        rig.poster.followups().len(),
        1,
        "unauthorized clicker gets an ephemeral rejection"
    );
    assert!(rig.poster.edits().is_empty(), "card not transitioned");
    assert_eq!(approval_status(&pool, id).await, "pending");
}

#[sqlx::test]
async fn double_click_is_idempotent(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let caller = Caller::new(seed.user_id, seed.org_id);
    let rig = build_rig(&pool, &caller, seed.agent_id).await;
    let id = seed_approval(&pool, &seed, ApproverPolicy::Anyone, "apv:i3").await;
    let dispatch = || interaction_dispatch(&format!("apv:{}:a", id.as_uuid()), CLICKER_ID);

    bridge::process_event(&rig.deps, dispatch())
        .await
        .expect("first click");
    bridge::process_event(&rig.deps, dispatch())
        .await
        .expect("second click");

    // Both clicks ack + re-render the resolved card; the row is approved once.
    assert_eq!(rig.poster.acks().len(), 2);
    assert_eq!(rig.poster.edits().len(), 2, "idempotent re-render");
    assert_eq!(approval_status(&pool, id).await, "approved");
}

#[sqlx::test]
async fn malformed_custom_id_is_a_noop(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let caller = Caller::new(seed.user_id, seed.org_id);
    let rig = build_rig(&pool, &caller, seed.agent_id).await;

    // A non-approval component and an `apv:` with a bad uuid both no-op before
    // any ack — they are not ours to handle.
    for custom_id in ["some_other_button", "apv:not-a-uuid:a"] {
        bridge::process_event(&rig.deps, interaction_dispatch(custom_id, CLICKER_ID))
            .await
            .expect("process interaction");
    }
    assert!(
        rig.poster.acks().is_empty(),
        "no ack for a foreign component"
    );
    assert!(rig.poster.edits().is_empty());
    assert!(rig.poster.followups().is_empty());
}
