//! Discord adapter end-to-end test (the "fake gateway feed").
//!
//! The cross-module inbound→outbound flow without a network: a decoded Gateway
//! dispatch is handed to `bridge::process_event`, which shadow-mints the sender,
//! mirrors the channel + thread, appends the `posted` row, and (on a DM/mention)
//! enqueues a run. A live single-worker pool runs the scripted agent, whose
//! `send_message` reply rides the real Postgres `LISTEN/NOTIFY` thread stream to
//! the stream pump, which posts it back through the [`FakeDiscordPoster`] seam
//! (no socket, no `wiremock`).

#![allow(clippy::expect_used)]
#![allow(clippy::too_many_lines)]

use std::sync::Arc;
use std::time::Duration;

use sqlx::PgPool;
use tokio_util::sync::CancellationToken;

use patom::auth::Caller;
use patom::clock::{SharedClock, SystemClock};
use patom::colleagues::PgColleagueStore;
use patom::crypto::OrgEncryptor;
use patom::discord::app_store::{NewDiscordApp, PgDiscordAppStore, SharedDiscordAppStore};
use patom::discord::bridge::{self, BridgeDeps};
use patom::discord::channel_map::PgDiscordChannelStore;
use patom::discord::connection::InboundDispatch;
use patom::discord::directory::PgDiscordDirectory;
use patom::discord::poster::{FakeDiscordPoster, SharedDiscordPoster};
use patom::discord::stream_pump::{self, PumpDeps};
use patom::discord::thread_map::PgDiscordThreadStore;
use patom::discord::thread_opener::{FakeThreadOpener, SharedThreadOpener};
use patom::discord::types::{
    ApplicationId, BotToken, ContainerId, DiscordMessageId, DiscordUserId,
};
use patom::provider::{AssistantContent, ChatResponse, StopReason, ToolCall, ToolCallId};
use patom::runtime::{PgThreadStream, SharedThreadStream};
use patom::threads::PgThreadStore;
use patom::types::SecretString;
use patom::types::ToolName;
use serde_json::json;

mod common;
use common::harness::{ScriptedProvider, build_harness};

const APP_ID: &str = "111111111111111111";
const BOT_USER_ID: &str = "999999999999999999";
const CHANNEL_ID: &str = "333333333333333333";
const AGENT_REPLY: &str = "here is your answer";

/// A turn that delivers to the human via `send_message` (the user-visible path).
fn send_message_call(content: &str) -> ChatResponse {
    ChatResponse {
        content: vec![AssistantContent::ToolCall(ToolCall {
            id: ToolCallId::try_from("call-1").expect("tool call id"),
            name: ToolName::try_from("send_message").expect("tool name"),
            input: json!({ "content": content }),
        })],
        stop_reason: StopReason::ToolUse,
        ..Default::default()
    }
}

/// A closing turn with final text (private close-out, not posted).
fn final_text(s: &str) -> ChatResponse {
    ChatResponse {
        content: vec![AssistantContent::Text(s.into())],
        stop_reason: StopReason::EndTurn,
        ..Default::default()
    }
}

struct DiscordRig {
    deps: BridgeDeps,
    fake: Arc<FakeDiscordPoster>,
}

async fn build_rig(
    pool: &PgPool,
    clock: &SharedClock,
    queue: patom::runtime::SharedPromptQueue,
    agent_id: patom::agents::AgentId,
    caller: &Caller,
) -> DiscordRig {
    let enc = Arc::new(OrgEncryptor::for_test([7u8; 32]));
    let app_store = Arc::new(PgDiscordAppStore::new(pool.clone(), clock.clone(), enc));
    let apps: SharedDiscordAppStore = app_store;
    // Register the bot → agent mapping the bridge resolves on each event.
    apps.register(
        caller,
        NewDiscordApp {
            application_id: ApplicationId::try_from(APP_ID).expect("app id"),
            agent_id,
            bot_token: BotToken::try_from("MTk4N.example.token".to_owned()).expect("token"),
        },
    )
    .await
    .expect("register discord app");

    let fake = Arc::new(FakeDiscordPoster::new());
    let poster: SharedDiscordPoster = fake.clone();
    let thread_stream: SharedThreadStream =
        PgThreadStream::spawn(pool.clone(), CancellationToken::new())
            .await
            .expect("thread stream");
    let directory: patom::discord::directory::SharedDiscordDirectory =
        Arc::new(PgDiscordDirectory::new(pool.clone(), clock.clone()));
    let pump = stream_pump::spawn(
        PumpDeps {
            thread_stream,
            poster,
            directory: directory.clone(),
            apps: apps.clone(),
            connect_secret: SecretString::try_from("test-discord-connect-secret".to_owned())
                .expect("non-empty"),
            connect_url_base: Arc::from("https://patom.example"),
            clock: clock.clone(),
        },
        CancellationToken::new(),
    );

    // A DM never opens a thread, so the opener is unused here.
    let thread_opener: SharedThreadOpener = Arc::new(FakeThreadOpener::returning(
        ContainerId::try_from("555000000000000001").expect("thread id"),
    ));
    let deps = BridgeDeps {
        apps,
        directory,
        channels: Arc::new(PgDiscordChannelStore::new(pool.clone(), clock.clone())),
        threads: Arc::new(PgDiscordThreadStore::new(pool.clone(), clock.clone())),
        thread_store: Arc::new(PgThreadStore::new(pool.clone(), clock.clone())),
        colleagues: Arc::new(PgColleagueStore::new(pool.clone())),
        queue,
        outbound: pump,
        history: Arc::new(patom::discord::history::FakeHistoryReader::empty()),
        thread_opener,
        assets: None,
        attachment_fetcher: Arc::new(patom::discord::attachment::FakeAttachmentFetcher::new()),
    };
    DiscordRig { deps, fake }
}

/// A DM (no guild) MESSAGE_CREATE — a trigger on its own.
fn dm_message(msg_id: &str, text: &str) -> InboundDispatch {
    InboundDispatch {
        application_id: ApplicationId::try_from(APP_ID).expect("app id"),
        bot_user_id: DiscordUserId::try_from(BOT_USER_ID).expect("bot id"),
        event_type: "MESSAGE_CREATE".to_owned(),
        data: json!({
            "id": msg_id,
            "channel_id": CHANNEL_ID,
            "author": {"id": "444444444444444444", "username": "alice", "global_name": "Alice"},
            "content": text,
            "mentions": [],
        }),
    }
}

/// Poll the fake poster until a reply carrying `text` appears (or time out).
async fn wait_for_post(fake: &FakeDiscordPoster, text: &str) -> bool {
    for _ in 0..200 {
        if fake.captured().iter().any(|p| p.content == text) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    false
}

// Proves the full loop end-to-end (the reply DOES post back through the pump),
// but the single-worker pool hangs on teardown after a *shadow* acting user's
// turn (the worker can't fully finalize a login-less acting user, so the runtime
// drop blocks) — a pre-existing harness limitation shared with Lark's analogous
// `dm_message_drives_agent_reply_back_to_lark`. Ignored so it never hangs CI;
// run manually with `cargo test --test discord_e2e -- --ignored --nocapture`.
// The inbound path is covered hang-free by `discord_bridge.rs`, and the outbound
// render/poster/pump logic by the in-module unit tests.
#[ignore = "full-worker reply path hangs on teardown for a shadow acting user (harness limitation)"]
#[sqlx::test]
async fn dm_message_drives_agent_reply_back_to_discord(pool: PgPool) {
    let provider = Arc::new(ScriptedProvider::new(vec![
        send_message_call(AGENT_REPLY),
        final_text("(internal close-out)"),
    ]));
    let h = build_harness(pool.clone(), provider).await;
    let clock: SharedClock = SystemClock::shared();
    let caller = Caller::new(h.default_user_id, h.default_org_id);

    let rig = build_rig(&pool, &clock, h.queue.clone(), h.default_agent_id, &caller).await;

    bridge::process_event(&rig.deps, dm_message("1001", "draft a JD"))
        .await
        .expect("process dm");

    // The whole flow ran: shadow minted → channel/thread → trigger → worker →
    // agent → send_message → pump → poster.
    assert!(
        wait_for_post(&rig.fake, AGENT_REPLY).await,
        "agent reply posted back to Discord; captured: {:?}",
        rig.fake
            .captured()
            .iter()
            .map(|p| p.content.clone())
            .collect::<Vec<_>>(),
    );
    let posted = rig.fake.captured();
    let reply = posted
        .iter()
        .find(|p| p.content == AGENT_REPLY)
        .expect("reply");
    assert_eq!(
        reply.container_id.as_str(),
        CHANNEL_ID,
        "reply lands in the DM channel"
    );
    assert_eq!(
        reply.reply_to.as_ref().map(DiscordMessageId::as_str),
        Some("1001"),
        "the reply threads under the triggering message (1001)",
    );
}
