//! End-to-end probe for the runtime-tenancy retrofit (migration 18).
//!
//! Three contracts, mirroring the prior probes
//! (`tests/auth_mcp_servers.rs`, `tests/auth_agents.rs`,
//! `tests/auth_threads.rs`, `tests/auth_memory.rs`):
//!
//!   1. Unauthenticated `POST /prompts` → 401.
//!   2. Authenticated `POST /prompts` → 202 with a valid `request_id`,
//!      `session_id`, and `pending` status.
//!   3. Cross-org isolation: a prompt enqueued under org A is
//!      invisible to org B — `POST /requests/{id}/cancel` issued by
//!      org B's principal returns 404, and the request row stays
//!      uncancelled.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::sync::Arc;

use patom_rs::agents::{
    AgentDescription, AgentName, AgentSystemPrompt, AllowedMcpTools, NewAgent, SharedAgentStore,
};
use patom_rs::clock::SystemClock;
use patom_rs::http::{AppState, router};
use patom_rs::mcp::{McpRefresher, McpRegistry, PgMcpServerStore, SharedMcpServerStore};
use patom_rs::runtime::{
    PgDagBudget, PgPromptQueue, PgResponseHub, PgThreadStream, SharedDagBudget, SharedLeaseManager,
    SharedPromptQueue, SharedResponseSink, SharedResponseSource, SharedThreadStream,
};
use patom_rs::session::{PgSessionStore, SharedSessionStore};
use sqlx::PgPool;
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;
use uuid::Uuid;

mod common;
use common::auth::{SeededPrincipal, seed_principal};
use common::pg::TestDb;

struct AuthPromptsHarness {
    state: AppState,
    agents: SharedAgentStore,
    primary: SeededPrincipal,
    #[allow(dead_code)]
    refresher: McpRefresher,
    #[allow(dead_code)]
    db: TestDb,
}

impl AuthPromptsHarness {
    async fn new() -> Self {
        let db = TestDb::fresh().await;
        let clock = SystemClock::shared();
        let pool: PgPool = db.pool.clone();

        let queue_impl = Arc::new(PgPromptQueue::new(pool.clone(), clock.clone()));
        let queue: SharedPromptQueue = queue_impl.clone();
        let leases: SharedLeaseManager = queue_impl;

        let hub = Arc::new(PgResponseHub::new(pool.clone(), clock.clone()));
        let _sink: SharedResponseSink = hub.clone();
        let responses: SharedResponseSource = hub;

        let sessions: SharedSessionStore =
            Arc::new(PgSessionStore::new(pool.clone(), clock.clone()));
        let agents: SharedAgentStore = common::pg::shared_agent_store(pool.clone(), clock.clone());
        let dag: SharedDagBudget = Arc::new(PgDagBudget::new(pool.clone()));

        let mcp_store: SharedMcpServerStore =
            Arc::new(PgMcpServerStore::new(pool.clone(), clock.clone()));
        let mcp_catalog: patom_rs::mcp::SharedMcpCatalogStore =
            Arc::new(patom_rs::mcp::PgMcpCatalogStore::new(pool.clone()));
        let mcp_registry = McpRegistry::new(mcp_store.clone(), clock.clone());
        let (refresher, mcp_refresh) = McpRefresher::spawn(mcp_registry);

        let thread_stream: SharedThreadStream =
            PgThreadStream::spawn(pool.clone(), CancellationToken::new())
                .await
                .expect("spawn thread stream");

        let memory_store: patom_rs::memory::SharedMemoryStore =
            Arc::new(patom_rs::memory::PgMemoryStore::new(
                pool.clone(),
                clock.clone(),
                common::embedding::FakeEmbeddingProvider::shared(),
            ));

        let jwt = common::auth::test_jwt(clock.clone());
        let oauth = common::auth::test_oauth();
        let users = common::auth::user_store(pool.clone());
        // Primary principal pinned to the seeded `default_org_id` so
        // the route's `agents.default_id_for(active_org_id)` fallback
        // resolves to the seeded default agent without needing a
        // per-test create.
        let primary =
            common::auth::principal_for_default_org(db.default_user_id, db.default_org_id, &jwt);

        let state = AppState {
            queue: queue.clone(),
            leases,
            responses,
            sessions,
            agents: agents.clone(),
            dag,
            memory_store,
            mcp_store,
            mcp_catalog,
            mcp_refresh,
            mcp_credentials: std::sync::Arc::new(patom_rs::mcp::PgMcpCredentialStore::new(
                pool.clone(),
                clock.clone(),
                std::sync::Arc::new(patom_rs::crypto::OrgEncryptor::for_test([0u8; 32])),
            )),
            mcp_test_rate: patom_rs::mcp::TestConnectRateLimiter::new(clock.clone()),
            platform_oauth_clients: std::sync::Arc::new(std::collections::HashMap::new()),
            mcp_oauth_pending: std::sync::Arc::new(
                patom_rs::mcp::oauth::PgMcpOAuthPendingStore::new(pool.clone(), clock.clone()),
            ),
            oauth_redirect_base: std::sync::Arc::from("http://localhost:8080"),
            web_base_url: None,
            thread_stream,
            pool: pool.clone(),
            jwt,
            oauth,
            users,
            clock: clock.clone(),
            cookie_secure: false,
            memberships: std::sync::Arc::new(patom_rs::http::MembershipCache::new(clock.clone())),
            prompts: common::lang::prompts(),
            language_resolver: common::lang::english_resolver(),
            rule_resolver: common::rule::empty_resolver(),
            web_dist: std::path::PathBuf::from("."),
            slack: None,
            assets: None,
            orgs: std::sync::Arc::new(patom_rs::orgs::PgOrgStore::new(pool.clone())),
            mailer: std::sync::Arc::new(patom_rs::orgs::LogMailer),
        };

        Self {
            state,
            agents,
            primary,
            refresher,
            db,
        }
    }

    /// Seed an agent owned by `org_id`. Used by the cross-org test so
    /// each principal has its own receiver — otherwise the enqueue
    /// against the seeded default agent (which lives in
    /// `db.default_org_id`) would 4xx for the second principal.
    async fn seed_agent(
        &self,
        org_id: patom_rs::auth::OrgId,
        name: &str,
    ) -> patom_rs::agents::AgentId {
        self.agents
            .create(NewAgent {
                org_id,
                name: AgentName::try_from(name).expect("name"),
                system_prompt: AgentSystemPrompt::try_from("scoped prompt").expect("prompt"),
                description: AgentDescription::try_from(format!("agent {name}")).expect("desc"),
                is_default: false,
                allowed_mcp_tools: AllowedMcpTools::empty(),
                model: None,
                edited_by: None,
            })
            .await
            .expect("seed agent")
            .id
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn unauthenticated_post_prompt_returns_401() {
    let h = AuthPromptsHarness::new().await;
    let app = router(h.state.clone());
    let res = app
        .oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri("/api/prompts")
                .header("content-type", "application/json")
                .body(axum::body::Body::from(
                    serde_json::json!({
                        "content": "hi",
                        "idempotency_key": "k-1",
                    })
                    .to_string(),
                ))
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(res.status(), axum::http::StatusCode::UNAUTHORIZED);
}

#[tokio::test(flavor = "multi_thread")]
async fn authenticated_post_prompt_returns_202_with_request_id() {
    let h = AuthPromptsHarness::new().await;
    let app = router(h.state.clone());
    let res = app
        .oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri("/api/prompts")
                .header("content-type", "application/json")
                .header("cookie", h.primary.cookie_header())
                .header("x-csrf-token", h.primary.csrf_header())
                .body(axum::body::Body::from(
                    serde_json::json!({
                        "content": "hello patom",
                        "idempotency_key": "k-primary-1",
                    })
                    .to_string(),
                ))
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(res.status(), axum::http::StatusCode::ACCEPTED);
    let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .expect("body");
    let json: serde_json::Value = serde_json::from_slice(&bytes).expect("json");
    assert!(
        json.get("request_id")
            .and_then(|v| v.as_str())
            .and_then(|s| Uuid::parse_str(s).ok())
            .is_some(),
        "response carries a valid request_id uuid",
    );
    assert!(
        json.get("session_id")
            .and_then(|v| v.as_str())
            .and_then(|s| Uuid::parse_str(s).ok())
            .is_some(),
        "response carries a valid session_id uuid",
    );
    assert_eq!(json.get("status").and_then(|v| v.as_str()), Some("pending"));
}

#[tokio::test(flavor = "multi_thread")]
async fn cross_org_cancel_returns_404_and_leaves_row_uncancelled() {
    let h = AuthPromptsHarness::new().await;
    // Distinct second principal in their own org. The seeded default
    // agent belongs to `db.default_org_id`, so the second principal
    // needs their own agent to enqueue against.
    let other = seed_principal(&h.state.pool, &h.state.jwt).await;
    let _other_agent = h.seed_agent(other.org_id, "beta").await;

    let app = router(h.state.clone());

    // Primary enqueues a prompt under their org.
    let res = app
        .clone()
        .oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri("/api/prompts")
                .header("content-type", "application/json")
                .header("cookie", h.primary.cookie_header())
                .header("x-csrf-token", h.primary.csrf_header())
                .body(axum::body::Body::from(
                    serde_json::json!({
                        "content": "primary prompt",
                        "idempotency_key": "k-primary-iso",
                    })
                    .to_string(),
                ))
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(res.status(), axum::http::StatusCode::ACCEPTED);
    let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .expect("body");
    let json: serde_json::Value = serde_json::from_slice(&bytes).expect("json");
    let request_id = json
        .get("request_id")
        .and_then(|v| v.as_str())
        .expect("request_id");

    // The *other* org's principal tries to cancel the primary's
    // request. RLS on `prompt_requests` hides it from them so the
    // tenant gate in the cancel handler resolves to NotFound — no
    // existence-leak.
    let res = app
        .clone()
        .oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri(format!("/api/requests/{request_id}/cancel"))
                .header("cookie", other.cookie_header())
                .header("x-csrf-token", other.csrf_header())
                .body(axum::body::Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(res.status(), axum::http::StatusCode::NOT_FOUND);

    // The row stays uncancelled — the gate returns before the
    // privileged cancellation write.
    let (cancelled,): (bool,) =
        sqlx::query_as("SELECT cancellation_requested FROM prompt_requests WHERE id = $1::uuid")
            .bind(request_id)
            .fetch_one(&h.state.pool)
            .await
            .expect("fetch");
    assert!(!cancelled, "cross-org cancel must not have mutated the row");

    // Sanity: the primary owner *can* cancel. Confirms the gate
    // isn't blanket-denying — only cross-org callers.
    let res = app
        .oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri(format!("/api/requests/{request_id}/cancel"))
                .header("cookie", h.primary.cookie_header())
                .header("x-csrf-token", h.primary.csrf_header())
                .body(axum::body::Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(res.status(), axum::http::StatusCode::NO_CONTENT);
    let (cancelled,): (bool,) =
        sqlx::query_as("SELECT cancellation_requested FROM prompt_requests WHERE id = $1::uuid")
            .bind(request_id)
            .fetch_one(&h.state.pool)
            .await
            .expect("fetch");
    assert!(cancelled, "owner's cancel landed");
}

// ---- CSRF middleware probes ---------------------------------------------
//
// These exercise the `require_csrf` layer applied inside the
// authenticated subtree. The middleware rejects state-changing requests
// whose `X-CSRF-Token` header doesn't match the `patom_csrf` cookie.
// Safe methods (GET) bypass the middleware entirely.

#[tokio::test(flavor = "multi_thread")]
async fn post_without_csrf_header_returns_403() {
    let h = AuthPromptsHarness::new().await;
    let app = router(h.state.clone());
    let res = app
        .oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri("/api/prompts")
                .header("content-type", "application/json")
                .header("cookie", h.primary.cookie_header())
                // Intentionally omit the x-csrf-token header.
                .body(axum::body::Body::from(
                    serde_json::json!({
                        "content": "csrf check",
                        "idempotency_key": "k-csrf-missing",
                    })
                    .to_string(),
                ))
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(res.status(), axum::http::StatusCode::FORBIDDEN);
}

#[tokio::test(flavor = "multi_thread")]
async fn post_with_mismatched_csrf_header_returns_403() {
    let h = AuthPromptsHarness::new().await;
    let app = router(h.state.clone());
    let res = app
        .oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri("/api/prompts")
                .header("content-type", "application/json")
                .header("cookie", h.primary.cookie_header())
                .header("x-csrf-token", "different-token")
                .body(axum::body::Body::from(
                    serde_json::json!({
                        "content": "csrf check",
                        "idempotency_key": "k-csrf-mismatch",
                    })
                    .to_string(),
                ))
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(res.status(), axum::http::StatusCode::FORBIDDEN);
}

#[tokio::test(flavor = "multi_thread")]
async fn get_without_csrf_passes_through() {
    // GET is exempt from the CSRF middleware — it's not a state-
    // changing method. /agents is a tenant-scoped GET that exercises
    // the same `private` subtree as the POST cases above.
    let h = AuthPromptsHarness::new().await;
    let app = router(h.state.clone());
    let res = app
        .oneshot(
            axum::http::Request::builder()
                .method("GET")
                .uri("/api/agents")
                .header("cookie", h.primary.cookie_header())
                .body(axum::body::Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(res.status(), axum::http::StatusCode::OK);
}
