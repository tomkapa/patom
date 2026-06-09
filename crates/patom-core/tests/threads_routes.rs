//! Integration tests for the chat-thread HTTP routes (G1, G2, G3).
//!
//! Boots an `AppState` against a fresh schema-per-test pool, builds the axum
//! `router` with the same wiring as production, and pokes it via
//! `tower::ServiceExt::oneshot`. Assertions cover:
//!
//! - G1: the channel feed lists every human-rooted DAG.
//! - G2: the flat thread history is empty for a fresh enqueue with no
//!   appended `session_messages`.
//! - G3 (NOTIFY): a chunk published via [`PgResponseHub`] arrives on the
//!   live thread stream subscriber for the matching root.

#![allow(clippy::expect_used)]

use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use patom::agents::SharedAgentStore;
use patom::clock::{SharedClock, SystemClock};
use patom::http::{AppState, router};
use patom::mcp::{McpRefresher, McpRegistry, PgMcpServerStore, SharedMcpServerStore};
use patom::provider::{ChatMessage, UserContent};
use patom::runtime::{
    IdempotencyKey, NewPromptRequest, PgDagBudget, PgPromptQueue, PgResponseHub, PgThreadStream,
    PromptRequestId, ResponseChunk, SharedDagBudget, SharedLeaseManager, SharedPromptQueue,
    SharedResponseSink, SharedResponseSource, SharedThreadStream, ThreadStreamEvent,
};
use patom::session::{PgSessionStore, SharedSessionStore};
use patom::types::Prompt;
use sqlx::PgPool;
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;

mod common;
use common::pg::{Seed, seed_tenant};

/// Minimal HTTP harness wired with every collaborator the threads routes
/// touch, plus the live `PgThreadStream` so G3 NOTIFY round-trips through
/// the real listener task.
struct ThreadsHarness {
    seed: Seed,
    queue: SharedPromptQueue,
    sink: SharedResponseSink,
    thread_stream: SharedThreadStream,
    state: AppState,
    /// `Cookie:` header carrying a valid JWT for the seeded test
    /// principal. Threaded into every request so the auth layer admits
    /// the call.
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
        let leases: SharedLeaseManager = queue_impl;

        let hub = Arc::new(PgResponseHub::new(pool.clone(), clock.clone()));
        let sink: SharedResponseSink = hub.clone();
        let responses: SharedResponseSource = hub;

        let sessions: SharedSessionStore =
            Arc::new(PgSessionStore::new(pool.clone(), clock.clone()));
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
            leases,
            responses,
            sessions,
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

        // The threads we enqueue belong to `seed.org_id`, so the
        // principal we mint must be a member of that same org —
        // otherwise the new RLS policies on `sessions` /
        // `session_messages` filter every row out of the response.
        let seeded = common::auth::principal_for_default_org(seed.user_id, seed.org_id, &state.jwt);

        Self {
            seed,
            queue,
            sink,
            thread_stream,
            state,
            refresher,
            auth_cookie: seeded.cookie_header(),
        }
    }
}

async fn enqueue_human_root(harness: &ThreadsHarness, content: &str, key: &str) -> PromptRequestId {
    harness
        .queue
        .enqueue(NewPromptRequest {
            session: None,
            sender: common::pg::human_participant(
                &harness.state.pool,
                harness.seed.org_id,
                harness.seed.user_id,
            )
            .await,
            receiver_agent_id: harness.seed.agent_id,
            parent_session: None,
            content: Prompt::try_from(content).expect("prompt"),
            idempotency_key: IdempotencyKey::try_from(key).expect("key"),
            org_id: harness.seed.org_id,
            created_by_user_id: harness.seed.user_id,
            kind_payload: patom::runtime::RequestKindPayload::Normal {},
        })
        .await
        .expect("enqueue")
        .request_id()
}

/// Seed a second human member of the harness org with a known
/// `display_name` and avatar, returning their `user_id`. The
/// `org_members` insert fires the colleague-mint trigger, so the new
/// member is immediately addressable as a human colleague.
async fn seed_member(
    pool: &PgPool,
    org_id: patom::auth::OrgId,
    display_name: &str,
    avatar_url: &str,
) -> patom::auth::UserId {
    let user_id = patom::auth::UserId::new();
    let now = chrono::Utc::now();
    let email = format!("member-{}@example.test", uuid::Uuid::new_v4().simple());
    sqlx::query(
        "INSERT INTO users (id, email, display_name, avatar_url, created_at, updated_at) \
         VALUES ($1, $2, $3, $4, $5, $5)",
    )
    .bind(user_id)
    .bind(&email)
    .bind(display_name)
    .bind(avatar_url)
    .bind(now)
    .execute(pool)
    .await
    .expect("seed member user");
    sqlx::query(
        "INSERT INTO org_members (org_id, user_id, role, created_at) VALUES ($1, $2, 'member', $3)",
    )
    .bind(org_id)
    .bind(user_id)
    .bind(now)
    .execute(pool)
    .await
    .expect("seed member membership");
    user_id
}

/// Enqueue a human-rooted DAG attributed to a specific `user_id` (not the
/// harness's default seed user), so multi-author scenarios can be built.
async fn enqueue_human_root_as(
    harness: &ThreadsHarness,
    user_id: patom::auth::UserId,
    content: &str,
    key: &str,
) -> PromptRequestId {
    harness
        .queue
        .enqueue(NewPromptRequest {
            session: None,
            sender: common::pg::human_participant(
                &harness.state.pool,
                harness.seed.org_id,
                user_id,
            )
            .await,
            receiver_agent_id: harness.seed.agent_id,
            parent_session: None,
            content: Prompt::try_from(content).expect("prompt"),
            idempotency_key: IdempotencyKey::try_from(key).expect("key"),
            org_id: harness.seed.org_id,
            created_by_user_id: user_id,
            kind_payload: patom::runtime::RequestKindPayload::Normal {},
        })
        .await
        .expect("enqueue")
        .request_id()
}

/// G1 multi-user: the feed must show the *real* starter of each thread,
/// not the viewer. A thread started by Alice shows Alice even when fetched
/// by Bob — the legacy single-human assumption stamped the viewer onto
/// every row.
#[sqlx::test]
async fn list_threads_attributes_starter_to_real_author(pool: PgPool) {
    let h = ThreadsHarness::new(pool).await;
    // Bob is the harness seed user (display "Seeded Test User").
    let bob = h.seed.user_id;
    let alice = seed_member(
        &h.state.pool,
        h.seed.org_id,
        "Alice Anders",
        "https://h.test/a.png",
    )
    .await;

    let bob_root = enqueue_human_root_as(&h, bob, "from bob", "k-bob").await;
    let alice_root = enqueue_human_root_as(&h, alice, "from alice", "k-alice").await;

    // Starter attribution is a multi-user concern, which now lives in shared
    // channels — a channel-less root is a DM private to its creator, so Bob
    // would never see Alice's there. Bob and Alice are both auto-enrolled in
    // their org's #general, so post both roots into it and read that channel
    // feed as Bob.
    let general_id: uuid::Uuid = sqlx::query_scalar(
        "SELECT id FROM channels WHERE org_id = $1 AND name = 'general' AND archived_at IS NULL",
    )
    .bind(h.seed.org_id)
    .fetch_one(&h.state.pool)
    .await
    .expect("general channel seeded");
    for root in [bob_root, alice_root] {
        sqlx::query("UPDATE prompt_requests SET channel_id = $1 WHERE id = $2")
            .bind(general_id)
            .bind(root)
            .execute(&h.state.pool)
            .await
            .expect("stamp channel");
    }

    // Fetched as Bob (the harness principal).
    let app = router(h.state.clone());
    let res = app
        .oneshot(
            axum::http::Request::builder()
                .uri(format!("/api/threads?channel_id={general_id}").as_str())
                .header("cookie", &h.auth_cookie)
                .body(axum::body::Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(res.status(), axum::http::StatusCode::OK);
    let body = axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .expect("collect");
    let json: serde_json::Value = serde_json::from_slice(&body).expect("json");
    let rows = json.as_array().expect("array");

    let row_for = |root: PromptRequestId| {
        rows.iter()
            .find(|r| r["root_request_id"].as_str() == Some(root.to_string().as_str()))
            .unwrap_or_else(|| panic!("row for {root} present"))
    };

    let alice_row = row_for(alice_root);
    assert_eq!(
        alice_row["starter"]["name"].as_str(),
        Some("Alice Anders"),
        "Alice's thread shows Alice as starter, not the viewer Bob",
    );
    assert_eq!(
        alice_row["starter"]["user_id"].as_str(),
        Some(alice.as_uuid().to_string().as_str()),
    );
    assert_eq!(
        alice_row["starter"]["avatar_url"].as_str(),
        Some("https://h.test/a.png"),
    );

    let bob_row = row_for(bob_root);
    assert_eq!(
        bob_row["starter"]["name"].as_str(),
        Some("Seeded Test User"),
        "Bob's thread shows Bob",
    );
    assert_eq!(
        bob_row["starter"]["user_id"].as_str(),
        Some(bob.as_uuid().to_string().as_str()),
    );
}

/// G2 multi-user: each human history row carries the *sender's* display
/// name and avatar, so a thread started by Alice shows Alice on her row
/// even when read by Bob.
#[sqlx::test]
async fn thread_messages_carry_sender_identity(pool: PgPool) {
    let h = ThreadsHarness::new(pool).await;
    let alice = seed_member(
        &h.state.pool,
        h.seed.org_id,
        "Alice Anders",
        "https://h.test/a.png",
    )
    .await;
    let root = enqueue_human_root_as(&h, alice, "hello from alice", "k-alice").await;

    // Stand up Alice↔agent session bound to her DAG root and append her turn.
    let agent = common::pg::agent_participant(&h.state.pool, h.seed.org_id, h.seed.agent_id).await;
    let session = h
        .state
        .sessions
        .resolve_or_create_for_pair(
            root,
            common::pg::human_participant(&h.state.pool, h.seed.org_id, alice).await,
            agent,
            None,
            h.seed.org_id,
            alice,
        )
        .await
        .expect("create session");
    h.state
        .sessions
        .append(
            session,
            common::pg::human_sender(&h.state.pool, h.seed.org_id, alice).await,
            agent,
            ChatMessage::User(vec![UserContent::Text("hello from alice".into())]),
            root,
        )
        .await
        .expect("append");

    // Read as Bob (the harness principal) — the row must still name Alice.
    let app = router(h.state.clone());
    let res = app
        .oneshot(
            axum::http::Request::builder()
                .uri(format!("/api/threads/{}/messages", root.as_uuid()))
                .header("cookie", &h.auth_cookie)
                .body(axum::body::Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(res.status(), axum::http::StatusCode::OK);
    let body = axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .expect("collect");
    let json: serde_json::Value = serde_json::from_slice(&body).expect("json");
    let rows = json.as_array().expect("array");
    assert_eq!(rows.len(), 1, "one appended human row");
    assert_eq!(rows[0]["sender"]["kind"].as_str(), Some("human"));
    assert_eq!(
        rows[0]["sender_display_name"].as_str(),
        Some("Alice Anders"),
        "human row names its real sender, not the viewer",
    );
    assert_eq!(
        rows[0]["sender_avatar_url"].as_str(),
        Some("https://h.test/a.png"),
    );
}

#[sqlx::test]
async fn list_threads_returns_one_row_per_human_root(pool: PgPool) {
    let h = ThreadsHarness::new(pool).await;
    let r1 = enqueue_human_root(&h, "first", "k-1").await;
    let r2 = enqueue_human_root(&h, "second", "k-2").await;

    let app = router(h.state.clone());
    let res = app
        .oneshot(
            axum::http::Request::builder()
                .uri("/api/threads")
                .header("cookie", &h.auth_cookie)
                .body(axum::body::Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(res.status(), axum::http::StatusCode::OK);

    let body = axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .expect("collect");
    let json: serde_json::Value = serde_json::from_slice(&body).expect("json");
    let rows = json.as_array().expect("array");
    assert_eq!(rows.len(), 2, "two human roots → two thread rows");

    let ids: Vec<String> = rows
        .iter()
        .map(|r| r["root_request_id"].as_str().expect("uuid").to_string())
        .collect();
    assert!(ids.contains(&r1.to_string()));
    assert!(ids.contains(&r2.to_string()));

    for r in rows {
        assert_eq!(r["reply_count"].as_i64(), Some(0));
        assert_eq!(r["status"].as_str(), Some("pending"));
        assert!(r["first_agent"]["name"].is_string());
        assert!(r["preview"].is_string());
    }
}

#[sqlx::test]
async fn thread_messages_returns_empty_for_fresh_dag(pool: PgPool) {
    let h = ThreadsHarness::new(pool).await;
    let root = enqueue_human_root(&h, "hello", "k-1").await;

    let app = router(h.state.clone());
    let res = app
        .oneshot(
            axum::http::Request::builder()
                .uri(format!("/api/threads/{}/messages", root.as_uuid()))
                .header("cookie", &h.auth_cookie)
                .body(axum::body::Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(res.status(), axum::http::StatusCode::OK);

    let body = axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .expect("collect");
    let json: serde_json::Value = serde_json::from_slice(&body).expect("json");
    assert_eq!(
        json.as_array().expect("array").len(),
        0,
        "fresh enqueue has no session_messages",
    );
}

/// Every appended `session_messages` row carries the `request_id` that
/// produced it, and the G2 read endpoint surfaces it on the wire so the FE
/// can dedupe optimistic / live / persisted bubbles by identity (no text
/// matching). See `doc/thread_panel_refactor_export.md` for the rationale.
#[sqlx::test]
async fn thread_messages_includes_request_id_per_row(pool: PgPool) {
    let h = ThreadsHarness::new(pool).await;
    let root = enqueue_human_root(&h, "hello", "k-1").await;

    // Stand up the human↔agent session bound to the enqueued DAG root, then
    // append one human turn carrying the same request_id the queue minted.
    let agent = common::pg::agent_participant(&h.state.pool, h.seed.org_id, h.seed.agent_id).await;
    let session = h
        .state
        .sessions
        .resolve_or_create_for_pair(
            root,
            common::pg::human_participant(&h.state.pool, h.seed.org_id, h.seed.user_id).await,
            agent,
            None,
            h.seed.org_id,
            h.seed.user_id,
        )
        .await
        .expect("create session");
    h.state
        .sessions
        .append(
            session,
            common::pg::human_sender(&h.state.pool, h.seed.org_id, h.seed.user_id).await,
            agent,
            ChatMessage::User(vec![UserContent::Text("hello".into())]),
            root,
        )
        .await
        .expect("append");

    let app = router(h.state.clone());
    let res = app
        .oneshot(
            axum::http::Request::builder()
                .uri(format!("/api/threads/{}/messages", root.as_uuid()))
                .header("cookie", &h.auth_cookie)
                .body(axum::body::Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(res.status(), axum::http::StatusCode::OK);

    let body = axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .expect("collect");
    let json: serde_json::Value = serde_json::from_slice(&body).expect("json");
    let rows = json.as_array().expect("array");
    assert_eq!(rows.len(), 1, "one appended row → one history row");
    assert_eq!(
        rows[0]["request_id"].as_str(),
        Some(root.as_uuid().to_string().as_str()),
        "history row must carry the request_id that produced it",
    );
}

#[sqlx::test]
async fn notify_drives_thread_stream_subscriber(pool: PgPool) {
    let h = ThreadsHarness::new(pool).await;
    let root = enqueue_human_root(&h, "hi", "k-1").await;

    // Subscribe to the live fan-in stream BEFORE publishing so the slot is
    // attached and `handle_notification` doesn't drop the chunk.
    let mut stream = h.thread_stream.subscribe(root);

    h.sink
        .publish(
            root,
            ResponseChunk::Text {
                value: "hello human".into(),
            },
        )
        .await
        .expect("publish");

    let item = tokio::time::timeout(Duration::from_secs(2), stream.next())
        .await
        .expect("notification arrived")
        .expect("stream item")
        .expect("ok");

    match item {
        ThreadStreamEvent::Item(ev) => {
            assert_eq!(ev.request_id, root);
            assert!(matches!(ev.chunk, ResponseChunk::Text { .. }));
            assert_eq!(ev.from_agent, h.seed.agent_id);
        }
        ThreadStreamEvent::Stalled => panic!("unexpected stalled event"),
    }
}
