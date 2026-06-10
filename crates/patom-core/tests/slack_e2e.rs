//! Slack adapter end-to-end test (GitHub issue #46).
//!
//! The Phase 1 PR shipped 49 unit/integration tests covering every pure
//! Slack module in isolation. The deferred gap — closed here — is the
//! cross-module flow: a *signed* `app_mention` webhook fired through the
//! real axum `router`, verified by the HMAC gate, handed to the bridge
//! worker over its bounded mpsc, enqueued, run by a live single-worker
//! pool against a scripted provider, and finally posted back to Slack —
//! asserting the outbound `chat.postMessage` carries the agent's
//! `username` and the originating message's `thread_ts`.
//!
//! Two deliberate fidelity choices:
//! - Outbound HTTP is the production [`FakeSlackPoster`] seam (no network,
//!   no `wiremock` dependency); `SLACK_POST_URL` is a `const`, so poster
//!   injection — not env override — is the supported test hook.
//! - The worker→pump hand-off rides the real Postgres `LISTEN/NOTIFY`
//!   thread stream. The worker (via `build_harness`) and the Slack
//!   bridge/pump/HTTP layer therefore share state through Postgres, not
//!   through shared `Arc`s — the same decoupling production relies on.

#![allow(clippy::expect_used)]

use std::sync::Arc;
use std::time::Duration;

use hmac::{Hmac, Mac};
use sha2::Sha256;
use sqlx::PgPool;
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;

use patom::agents::SharedAgentStore;
use patom::clock::{SharedClock, SystemClock};
use patom::http::{AppState, router};
use patom::mcp::{McpRefresher, McpRegistry, PgMcpServerStore, SharedMcpServerStore};
use patom::provider::{AssistantContent, ChatResponse, StopReason, ToolCall, ToolCallId};
use patom::runtime::{
    PgDagBudget, PgPromptQueue, PgResponseHub, PgThreadStream, SharedDagBudget, SharedPromptQueue,
    SharedResponseSource, SharedThreadStream,
};
use patom::slack::SlackAppState;
use patom::slack::bridge::{self, BridgeDeps};
use patom::slack::identity::{PgSlackIdentityStore, SharedSlackIdentityStore};
use patom::slack::poster::{FakeSlackPoster, PostRequest, SharedSlackPoster};
use patom::slack::stream_pump::{self, PumpDeps};
use patom::slack::thread_map::{PgSlackThreadStore, SharedSlackThreadStore};
use patom::slack::types::{SlackBotToken, SlackTeamId, SlackThreadTs, SlackUserId};
use patom::slack::workspace::{NewWorkspace, PgSlackWorkspaceStore, SharedSlackWorkspaceStore};
use patom::types::{SecretString, ToolName};
use serde_json::json;

mod common;
use common::harness::{ScriptedProvider, build_harness};

/// A turn that delivers to the human via `send_message` — the spec's
/// delivery path (a plain `Done.final_text` is treated as a private
/// thought, not a user-visible message).
fn send_message_call(content: &str) -> ChatResponse {
    ChatResponse {
        content: vec![AssistantContent::ToolCall(ToolCall {
            id: ToolCallId::try_from("call-1").expect("tool call id"),
            name: ToolName::try_from("send_message").expect("tool name"),
            input: json!({ "receiver": { "kind": "human" }, "content": content }),
        })],
        stop_reason: StopReason::ToolUse,
        ..Default::default()
    }
}

/// A closing turn with final text. `send_message` was already called, so
/// the worker accepts the success path (no ping-pong retry).
fn final_text(s: &str) -> ChatResponse {
    ChatResponse {
        content: vec![AssistantContent::Text(s.into())],
        stop_reason: StopReason::EndTurn,
        ..Default::default()
    }
}

/// Canonical Slack signature over `v0:<ts>:<body>` — mirrors what Slack
/// stamps on every webhook and what `slack::verify` recomputes.
fn slack_signature(secret: &str, ts: i64, body: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("hmac key init");
    mac.update(b"v0:");
    mac.update(ts.to_string().as_bytes());
    mac.update(b":");
    mac.update(body);
    let out = mac.finalize().into_bytes();
    let mut hex = String::with_capacity(67);
    hex.push_str("v0=");
    for byte in out {
        write!(hex, "{byte:02x}").expect("write to in-memory String is infallible");
    }
    hex
}

/// Poll the fake poster until a post whose fallback text equals `text`
/// arrives, or `timeout` elapses. The agent's reply travels webhook →
/// bridge → queue → worker → NOTIFY → pump before it lands, so the wait
/// is generous and matches the timing model the rest of the suite uses.
async fn wait_for_post(
    poster: &FakeSlackPoster,
    text: &str,
    timeout: Duration,
) -> Option<PostRequest> {
    let until = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < until {
        if let Some(p) = poster
            .captured()
            .into_iter()
            .find(|p| p.body.fallback_text() == text)
        {
            return Some(p);
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    poster
        .captured()
        .into_iter()
        .find(|p| p.body.fallback_text() == text)
}

#[sqlx::test]
#[allow(clippy::too_many_lines)] // one composition root, no branching — mirrors app.rs
async fn signed_app_mention_drives_agent_reply_back_to_slack(pool: PgPool) {
    const TEAM_ID: &str = "T0E2ETEST";
    const CHANNEL_ID: &str = "C0E2ECHAN";
    const HUMAN_USER: &str = "U0HUMAN";
    const BOT_USER: &str = "U0BOT";
    const EVENT_TS: &str = "1700000000.000100";
    const AGENT_REPLY: &str = "here is your answer";
    const SIGNING_SECRET: &str = "e2e-slack-signing-secret-key-0001";

    // Worker: scripted provider delivers to the human, then closes the
    // turn. `build_harness` seeds the tenant (org/user/`test-default`
    // agent) and spawns a single-worker pool polling `pool`.
    let provider = Arc::new(ScriptedProvider::new(vec![
        send_message_call(AGENT_REPLY),
        final_text("(internal close-out)"),
    ]));
    let h = build_harness(pool.clone(), provider).await;

    let clock: SharedClock = SystemClock::shared();

    // Queue / session / response stores — the bridge enqueues into the
    // same Postgres the worker polls.
    let queue: SharedPromptQueue = Arc::new(PgPromptQueue::new(pool.clone(), clock.clone()));
    let hub = Arc::new(PgResponseHub::new(pool.clone(), clock.clone()));
    let responses: SharedResponseSource = hub;
    let agents: SharedAgentStore = common::pg::shared_agent_store(pool.clone(), clock.clone());
    let dag: SharedDagBudget = Arc::new(PgDagBudget::new(pool.clone()));

    let mcp_store: SharedMcpServerStore =
        Arc::new(PgMcpServerStore::new(pool.clone(), clock.clone()));
    let mcp_catalog: patom::mcp::SharedMcpCatalogStore =
        Arc::new(patom::mcp::PgMcpCatalogStore::new(
            pool.clone(),
            ::std::sync::Arc::new(patom::crypto::OrgEncryptor::for_test([0u8; 32])),
        ));
    let mcp_registry = McpRegistry::new(mcp_store.clone(), clock.clone());
    let (_refresher, mcp_refresh) = McpRefresher::spawn(mcp_registry);

    let memory_store: patom::memory::SharedMemoryStore =
        Arc::new(patom::memory::PgMemoryStore::new(
            pool.clone(),
            clock.clone(),
            common::embedding::FakeEmbeddingProvider::shared(),
        ));
    let jwt = common::auth::test_jwt(clock.clone());
    let oauth = common::auth::test_oauth();
    let users = common::auth::user_store(pool.clone());

    // Slack stores (workspace / identity / thread) plus the install row.
    // One `OrgEncryptor` instance seals on upsert and opens on read.
    let enc: patom::crypto::SharedOrgEncryptor =
        Arc::new(patom::crypto::OrgEncryptor::for_test([7u8; 32]));
    let workspaces: SharedSlackWorkspaceStore =
        Arc::new(PgSlackWorkspaceStore::new(pool.clone(), clock.clone(), enc));
    let identities: SharedSlackIdentityStore =
        Arc::new(PgSlackIdentityStore::new(pool.clone(), clock.clone()));
    let slack_threads: SharedSlackThreadStore =
        Arc::new(PgSlackThreadStore::new(pool.clone(), clock.clone()));
    let thread_store: patom::threads::SharedThreadStore = Arc::new(
        patom::threads::PgThreadStore::new(pool.clone(), clock.clone()),
    );

    // An un-named `@Patom` mention routes to the org's preset recruiter
    // (there is no default agent). The harness seed is named `test-default`,
    // so rename it to the preset name the bridge resolves.
    agents
        .update(
            h.default_agent_id,
            patom::agents::AgentUpdate {
                name: Some(
                    patom::agents::AgentName::try_from(patom::app::RECRUITER_AGENT_NAME)
                        .expect("recruiter name"),
                ),
                ..Default::default()
            },
        )
        .await
        .expect("rename seed agent to recruiter");

    let principal =
        common::auth::principal_for_default_org(h.default_user_id, h.default_org_id, &jwt)
            .as_principal();
    workspaces
        .upsert(
            &principal,
            NewWorkspace {
                org_id: h.default_org_id,
                team_id: SlackTeamId::try_from(TEAM_ID).expect("team id"),
                team_name: "E2E Workspace".to_owned(),
                bot_user_id: SlackUserId::try_from(BOT_USER).expect("bot id"),
                bot_token: SlackBotToken::try_from("xoxb-e2e-test-token".to_owned())
                    .expect("bot token"),
                scopes: "app_mentions:read,chat:write".to_owned(),
                installed_by_user_id: h.default_user_id,
            },
        )
        .await
        .expect("seed slack workspace");

    // Outbound poster fake + the live NOTIFY thread stream the pump rides.
    let fake = Arc::new(FakeSlackPoster::new());
    let poster: SharedSlackPoster = fake.clone();
    let thread_stream: SharedThreadStream =
        PgThreadStream::spawn(pool.clone(), CancellationToken::new())
            .await
            .expect("thread stream");

    // Shared one-per-purpose values threaded into the bridge, pump, and
    // HTTP layer (the reqwest client and signing secret are cheap-clone).
    let http = reqwest::Client::new();
    let signing_secret = SecretString::try_from(SIGNING_SECRET.to_owned()).expect("secret");

    // Slack workers: stream pump (outbound) + inbound bridge.
    let slack_cancel = CancellationToken::new();
    let pump = stream_pump::spawn(
        PumpDeps {
            thread_stream: thread_stream.clone(),
            workspaces: workspaces.clone(),
            agents: agents.clone(),
            poster: poster.clone(),
            signing_secret: signing_secret.clone(),
            connect_url_base: Arc::from("https://patom.example"),
            clock: clock.clone(),
        },
        slack_cancel.clone(),
    );
    let (bridge_handle, bridge_tx) = bridge::spawn(
        BridgeDeps {
            queue: queue.clone(),
            agents: agents.clone(),
            thread_store: thread_store.clone(),
            colleagues: std::sync::Arc::new(patom::colleagues::PgColleagueStore::new(pool.clone())),
            workspaces: workspaces.clone(),
            identities: identities.clone(),
            threads: slack_threads.clone(),
            poster: poster.clone(),
            stream_pump: pump.clone(),
            pool: pool.clone(),
            http: http.clone(),
        },
        slack_cancel.clone(),
    );

    let slack_state = SlackAppState {
        signing_secret,
        client_id: Arc::from("test-client-id"),
        client_secret: SecretString::try_from("test-client-secret".to_owned()).expect("secret"),
        redirect_url: Arc::from("https://patom.example/slack/oauth/callback"),
        workspaces: workspaces.clone(),
        identities: identities.clone(),
        threads: slack_threads.clone(),
        poster: poster.clone(),
        http,
        bridge_tx,
        stream_pump: pump.clone(),
        clock: clock.clone(),
    };

    let state = AppState {
        queue: queue.clone(),
        responses,
        agents: agents.clone(),
        colleagues: std::sync::Arc::new(patom::colleagues::PgColleagueStore::new(pool.clone())),

        dag,
        billing: Arc::new(patom::billing::PgBillingService::new(
            pool.clone(),
            SystemClock::shared(),
        )),
        memory_store,
        mcp_store,
        mcp_catalog,
        mcp_refresh,
        provider_credentials: common::pg::provider_credentials_store(pool.clone()),
        provider_refresh: patom::provider::ProviderRefreshTrigger::disconnected(),
        mcp_credentials: Arc::new(patom::mcp::PgMcpCredentialStore::new(
            pool.clone(),
            clock.clone(),
            Arc::new(patom::crypto::OrgEncryptor::for_test([0u8; 32])),
        )),
        mcp_test_rate: patom::mcp::TestConnectRateLimiter::new(clock.clone()),
        platform_oauth_clients: Arc::new(std::collections::HashMap::new()),
        mcp_oauth_pending: Arc::new(patom::mcp::oauth::PgMcpOAuthPendingStore::new(
            pool.clone(),
            clock.clone(),
        )),
        oauth_redirect_base: Arc::from("http://localhost:8080"),
        web_base_url: None,
        thread_stream: thread_stream.clone(),
        pool: pool.clone(),
        jwt,
        oauth,
        bootstrap_admin: false,
        cloud: false,
        users,
        clock: clock.clone(),
        cookie_secure: false,
        cookie_domain: None,
        cors_allowed_origins: Vec::new(),
        memberships: Arc::new(patom::http::MembershipCache::new(clock.clone())),
        prompts: common::lang::prompts(),
        language_resolver: common::lang::english_resolver(),
        rule_resolver: common::rule::empty_resolver(),
        web_dist: std::path::PathBuf::from("."),
        index_html: std::sync::Arc::from(""),
        slack: Some(slack_state),
        assets: None,
        orgs: Arc::new(patom::orgs::PgOrgStore::new(pool.clone())),
        mailer: Arc::new(patom::orgs::LogMailer),
        entitlements: Arc::new(patom::entitlements::UnlimitedEntitlements),
    };

    // Fire the signed `app_mention` webhook through the real router. The
    // signature is computed over the exact bytes sent; the timestamp is
    // `now` so the freshness gate admits it.
    let body = json!({
        "type": "event_callback",
        "team_id": TEAM_ID,
        "event": {
            "type": "app_mention",
            "channel": CHANNEL_ID,
            "user": HUMAN_USER,
            "text": format!("<@{BOT_USER}> answer my question"),
            "ts": EVENT_TS,
        },
    })
    .to_string()
    .into_bytes();
    let ts = chrono::Utc::now().timestamp();
    let signature = slack_signature(SIGNING_SECRET, ts, &body);

    let res = router(state.clone())
        .oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri("/slack/events")
                .header("content-type", "application/json")
                .header("X-Slack-Request-Timestamp", ts.to_string())
                .header("X-Slack-Signature", signature)
                .body(axum::body::Body::from(body))
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(
        res.status(),
        axum::http::StatusCode::OK,
        "signed webhook is verified and acked",
    );

    // The worker runs the scripted turn; the pump posts the reply back to
    // the thread the bridge bound to the mention's `ts`.
    let post = wait_for_post(&fake, AGENT_REPLY, Duration::from_secs(15))
        .await
        .unwrap_or_else(|| {
            panic!(
                "expected a chat.postMessage carrying {AGENT_REPLY:?}; captured={:?}",
                fake.captured(),
            )
        });

    let routed_agent = agents
        .read(h.default_agent_id)
        .await
        .expect("read seeded agent");
    assert_eq!(
        post.username,
        routed_agent.name.as_str(),
        "attributed to the routed agent's name",
    );
    assert_eq!(
        post.channel.as_str(),
        CHANNEL_ID,
        "posted into the originating channel",
    );
    assert_eq!(
        post.thread_ts.as_ref().map(SlackThreadTs::as_str),
        Some(EVENT_TS),
        "threaded under the mention's ts",
    );
    assert_eq!(post.body.fallback_text(), AGENT_REPLY);

    // Teardown: stop the live tasks so the runtime drains cleanly.
    slack_cancel.cancel();
    bridge_handle.shutdown().await;
    pump.shutdown().await;
    h.workers.shutdown().await;
}
