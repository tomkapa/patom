//! Integration tests for the chat-thread HTTP routes (G1, G2, G3) in the
//! thread-feed model.
//!
//! Boots an `AppState` against a fresh schema-per-test pool, builds the axum
//! `router` with the same wiring as production, and drives it via
//! `tower::ServiceExt::oneshot`. The flow under test is the real one: `POST
//! /prompts` creates a thread + appends the human's posted row + enqueues a
//! trigger, then the read routes surface it.
//!
//! - G1: `GET /threads` lists the caller's threads (`{thread_id, channel_id,
//!   last_activity_at}`), one per `POST /prompts` root.
//! - G2: `GET /threads/{id}/messages` returns the flat feed — the human's
//!   posted row with its real sender identity.
//! - G3 (NOTIFY): a chunk published via [`PgResponseHub`] on a request in the
//!   thread arrives on the live thread-stream subscriber keyed by `thread_id`.

#![allow(clippy::expect_used)]

use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use patom::agents::SharedAgentStore;
use patom::clock::{SharedClock, SystemClock};
use patom::http::{AppState, router};
use patom::mcp::{McpRefresher, McpRegistry, PgMcpServerStore, SharedMcpServerStore};
use patom::runtime::{
    PgDagBudget, PgPromptQueue, PgResponseHub, PgThreadStream, PromptRequestId, ResponseChunk,
    SharedDagBudget, SharedPromptQueue, SharedResponseSink, SharedResponseSource,
    SharedThreadStream, ThreadStreamEvent,
};
use patom::threads::ThreadId;
use sqlx::PgPool;
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;
use uuid::Uuid;

mod common;
use common::pg::{Seed, seed_tenant};

/// Minimal HTTP harness wired with every collaborator the threads routes
/// touch, plus the live `PgThreadStream` so G3 NOTIFY round-trips through
/// the real listener task.
struct ThreadsHarness {
    seed: Seed,
    sink: SharedResponseSink,
    thread_stream: SharedThreadStream,
    state: AppState,
    /// `Cookie:` header carrying a valid JWT for the seeded test principal.
    auth_cookie: String,
    /// Held so its `Drop` reaps the coordinator task.
    #[allow(dead_code)]
    refresher: McpRefresher,
}

impl ThreadsHarness {
    // Composition root for a single integration test harness: 100+ lines
    // of wiring without branching, mirrors `app.rs::Collaborators::new`.
    #[allow(clippy::too_many_lines)]
    async fn new(pool: PgPool) -> Self {
        let seed = seed_tenant(&pool).await;
        let clock: SharedClock = SystemClock::shared();

        let queue_impl = Arc::new(PgPromptQueue::new(pool.clone(), clock.clone()));
        let queue: SharedPromptQueue = queue_impl.clone();

        let hub = Arc::new(PgResponseHub::new(pool.clone(), clock.clone()));
        let sink: SharedResponseSink = hub.clone();
        let responses: SharedResponseSource = hub;

        let agent_store: SharedAgentStore =
            common::pg::shared_agent_store(pool.clone(), clock.clone());
        let dag: SharedDagBudget = Arc::new(PgDagBudget::new(pool.clone()));

        let mcp_store: SharedMcpServerStore =
            Arc::new(PgMcpServerStore::new(pool.clone(), clock.clone()));
        let mcp_catalog: patom::mcp::SharedMcpCatalogStore =
            Arc::new(patom::mcp::PgMcpCatalogStore::new(pool.clone()));
        let mcp_registry = McpRegistry::new(mcp_store.clone(), clock.clone());
        let (refresher, mcp_refresh) = McpRefresher::spawn(mcp_registry);

        let thread_stream: SharedThreadStream =
            PgThreadStream::spawn(pool.clone(), CancellationToken::new())
                .await
                .expect("spawn thread stream");

        let memory_store: patom::memory::SharedMemoryStore =
            Arc::new(patom::memory::PgMemoryStore::new(
                pool.clone(),
                clock.clone(),
                common::embedding::FakeEmbeddingProvider::shared(),
            ));
        let jwt = common::auth::test_jwt(clock.clone());
        let oauth = common::auth::test_oauth();
        let users = common::auth::user_store(pool.clone());
        let state = AppState {
            queue: queue.clone(),
            responses,
            agents: agent_store,
            colleagues: std::sync::Arc::new(patom::colleagues::PgColleagueStore::new(pool.clone())),
            dag,
            budget: std::sync::Arc::new(patom::budget::PgBudgetService::new(
                pool.clone(),
                patom::clock::SystemClock::shared(),
            )),
            memory_store,
            mcp_store,
            mcp_catalog,
            mcp_refresh,
            mcp_credentials: std::sync::Arc::new(patom::mcp::PgMcpCredentialStore::new(
                pool.clone(),
                clock.clone(),
                std::sync::Arc::new(patom::crypto::OrgEncryptor::for_test([0u8; 32])),
            )),
            mcp_test_rate: patom::mcp::TestConnectRateLimiter::new(clock.clone()),
            platform_oauth_clients: std::sync::Arc::new(std::collections::HashMap::new()),
            mcp_oauth_pending: std::sync::Arc::new(patom::mcp::oauth::PgMcpOAuthPendingStore::new(
                pool.clone(),
                clock.clone(),
            )),
            oauth_redirect_base: std::sync::Arc::from("http://localhost:8080"),
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
            memberships: std::sync::Arc::new(patom::http::MembershipCache::new(clock.clone())),
            prompts: common::lang::prompts(),
            language_resolver: common::lang::english_resolver(),
            rule_resolver: common::rule::empty_resolver(),
            web_dist: std::path::PathBuf::from("."),
            slack: None,
            assets: None,
            orgs: std::sync::Arc::new(patom::orgs::PgOrgStore::new(pool.clone())),
            mailer: std::sync::Arc::new(patom::orgs::LogMailer),
            entitlements: std::sync::Arc::new(patom::entitlements::UnlimitedEntitlements),
        };

        let seeded = common::auth::principal_for_default_org(seed.user_id, seed.org_id, &state.jwt);

        Self {
            seed,
            sink,
            thread_stream,
            state,
            refresher,
            auth_cookie: seeded.cookie_header(),
        }
    }

    /// `POST /api/prompts` with the given body; returns `(request_id, thread_id)`
    /// from the response. Asserts a 2xx.
    async fn submit(&self, body: serde_json::Value) -> (PromptRequestId, ThreadId) {
        let app = router(self.state.clone());
        let res = app
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/api/prompts")
                    .header("cookie", &self.auth_cookie)
                    .header("content-type", "application/json")
                    .header(
                        patom::auth::limits::CSRF_HEADER_NAME,
                        common::auth::TEST_CSRF_TOKEN,
                    )
                    .body(axum::body::Body::from(
                        serde_json::to_vec(&body).expect("serialize body"),
                    ))
                    .expect("request"),
            )
            .await
            .expect("response");
        assert!(
            res.status().is_success(),
            "submit returned {}",
            res.status()
        );
        let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .expect("collect");
        let json: serde_json::Value = serde_json::from_slice(&bytes).expect("json");
        let request_id = PromptRequestId::from(
            Uuid::parse_str(json["request_id"].as_str().expect("request_id")).expect("uuid"),
        );
        let thread_id = ThreadId::from(
            Uuid::parse_str(json["thread_id"].as_str().expect("thread_id")).expect("uuid"),
        );
        (request_id, thread_id)
    }

    async fn get(&self, uri: &str) -> serde_json::Value {
        let app = router(self.state.clone());
        let res = app
            .oneshot(
                axum::http::Request::builder()
                    .uri(uri)
                    .header("cookie", &self.auth_cookie)
                    .body(axum::body::Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(res.status(), axum::http::StatusCode::OK, "GET {uri}");
        let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .expect("collect");
        serde_json::from_slice(&bytes).expect("json")
    }
}

/// A `POST /prompts` body: a DM (no channel) addressed to the seed agent.
fn dm_prompt(content: &str, key: &str) -> serde_json::Value {
    serde_json::json!({ "content": content, "idempotency_key": key })
}

#[sqlx::test]
async fn list_threads_returns_one_row_per_prompt_root(pool: PgPool) {
    let h = ThreadsHarness::new(pool).await;
    let (_r1, t1) = h.submit(dm_prompt("first", "k-1")).await;
    let (_r2, t2) = h.submit(dm_prompt("second", "k-2")).await;

    let json = h.get("/api/threads").await;
    let rows = json.as_array().expect("array");
    assert_eq!(rows.len(), 2, "two prompt roots → two DM thread rows");

    let ids: Vec<String> = rows
        .iter()
        .map(|r| r["thread_id"].as_str().expect("thread_id").to_string())
        .collect();
    assert!(ids.contains(&t1.as_uuid().to_string()));
    assert!(ids.contains(&t2.as_uuid().to_string()));
    for r in rows {
        assert!(r["last_activity_at"].is_string());
        // DM threads have no channel.
        assert!(r["channel_id"].is_null());
    }
}

#[sqlx::test]
async fn thread_messages_carries_human_sender_identity(pool: PgPool) {
    let h = ThreadsHarness::new(pool).await;
    let (_req, thread) = h.submit(dm_prompt("hello from the human", "k-1")).await;

    let json = h
        .get(&format!("/api/threads/{}/messages", thread.as_uuid()))
        .await;
    let rows = json.as_array().expect("array");
    assert_eq!(rows.len(), 1, "the human's posted row");
    let row = &rows[0];
    assert_eq!(row["kind"].as_str(), Some("posted"));
    assert_eq!(row["sender"]["kind"].as_str(), Some("human"));
    assert_eq!(
        row["sender_display_name"].as_str(),
        Some("Seeded Test User"),
        "human row names its real sender",
    );
    // The posted body carries the prompt text.
    assert!(
        serde_json::to_string(&row["body"])
            .expect("body json")
            .contains("hello from the human"),
    );
}

#[sqlx::test]
async fn notify_drives_thread_stream_subscriber_by_thread(pool: PgPool) {
    let h = ThreadsHarness::new(pool).await;
    let (request_id, thread) = h.submit(dm_prompt("hi", "k-1")).await;

    // Subscribe to the live fan-in for the thread BEFORE publishing so the slot
    // is attached and `handle_notification` doesn't drop the chunk.
    let mut stream = h.thread_stream.subscribe(thread);

    h.sink
        .publish(
            request_id,
            ResponseChunk::Text {
                value: "hello human".into(),
            },
        )
        .await
        .expect("publish");

    let item = tokio::time::timeout(Duration::from_secs(5), stream.next())
        .await
        .expect("notification arrived")
        .expect("stream item")
        .expect("ok");

    match item {
        ThreadStreamEvent::Item(ev) => {
            assert_eq!(ev.thread_id, thread);
            assert_eq!(ev.request_id, request_id);
            assert!(matches!(ev.chunk, ResponseChunk::Text { .. }));
            assert_eq!(ev.from_agent, h.seed.agent_id);
        }
        ThreadStreamEvent::Stalled => panic!("unexpected stalled event"),
    }
}
