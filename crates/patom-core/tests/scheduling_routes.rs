//! Integration tests for the Scheduled Tasks HTTP surface
//! (`GET /agents/{id}/scheduled-tasks`, `POST …/{tid}/cancel`).
//!
//! These pin the wire contract the FE "Scheduled Tasks" view consumes:
//! the `{ items, total, summary }` envelope, the `active|completed|
//! cancelled` status projection (mapping the store's `done`), the
//! `recurring|one_time` kind, the formatted cadence / run labels, and the
//! next-run-dash rule for non-active rows. Driven through the live axum
//! router + real Postgres so column / RLS drift surfaces here, not as a
//! runtime 500 the first time an operator opens the tab.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::sync::Arc;

use chrono::{DateTime, TimeZone, Utc};
use patom::agents::SharedAgentStore;
use patom::clock::{SharedClock, SystemClock};
use patom::http::{AppState, router};
use patom::mcp::{McpRefresher, McpRegistry, PgMcpServerStore, SharedMcpServerStore};
use patom::runtime::{
    PgDagBudget, PgPromptQueue, PgResponseHub, PgThreadStream, PromptRequestId, SharedDagBudget,
    SharedPromptQueue, SharedResponseSink, SharedResponseSource, SharedThreadStream,
};
use patom::scheduling::{
    NewScheduledTask, PgScheduledTaskStore, ScheduleSpec, ScheduledPrompt, ScheduledTaskId,
    ScheduledTaskName, ScheduledTaskStore, TimeOfDay, Timezone, Weekdays,
};
use sqlx::PgPool;
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;

mod common;
use common::auth::{SeededPrincipal, principal_for_default_org};
use common::pg::{Seed, seed_tenant};

// ─── harness ───────────────────────────────────────────────────────────

struct Harness {
    seed: Seed,
    state: AppState,
    primary: SeededPrincipal,
    sched: Arc<PgScheduledTaskStore>,
    clock: SharedClock,
}

impl Harness {
    async fn new(pool: PgPool) -> Self {
        let seed = seed_tenant(&pool).await;
        let clock: SharedClock = SystemClock::shared();

        let queue_impl = Arc::new(PgPromptQueue::new(pool.clone(), clock.clone()));
        let queue: SharedPromptQueue = queue_impl.clone();

        let hub = Arc::new(PgResponseHub::new(pool.clone(), clock.clone()));
        let _sink: SharedResponseSink = hub.clone();
        let responses: SharedResponseSource = hub;

        let agents: SharedAgentStore = common::pg::shared_agent_store(pool.clone(), clock.clone());
        let dag: SharedDagBudget = Arc::new(PgDagBudget::new(pool.clone()));

        let mcp_store: SharedMcpServerStore =
            Arc::new(PgMcpServerStore::new(pool.clone(), clock.clone()));
        let mcp_catalog: patom::mcp::SharedMcpCatalogStore =
            Arc::new(patom::mcp::PgMcpCatalogStore::new(pool.clone()));
        let mcp_registry = McpRegistry::new(mcp_store.clone(), clock.clone());
        let (_refresher, mcp_refresh) = McpRefresher::spawn(mcp_registry);

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
        let primary = principal_for_default_org(seed.user_id, seed.org_id, &jwt);
        let sched = Arc::new(PgScheduledTaskStore::new(pool.clone(), clock.clone()));

        let state = AppState {
            queue,
            responses,
            agents: agents.clone(),
            colleagues: Arc::new(patom::colleagues::PgColleagueStore::new(pool.clone())),
            dag,
            budget: Arc::new(patom::budget::PgBudgetService::new(
                pool.clone(),
                SystemClock::shared(),
            )),
            memory_store,
            mcp_store: mcp_store.clone(),
            mcp_catalog,
            mcp_refresh,
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
            thread_stream,
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
            slack: None,
            assets: None,
            orgs: Arc::new(patom::orgs::PgOrgStore::new(pool.clone())),
            mailer: Arc::new(patom::orgs::LogMailer),
            entitlements: Arc::new(patom::entitlements::UnlimitedEntitlements),
        };

        Self {
            seed,
            state,
            primary,
            sched,
            clock,
        }
    }

    /// Insert one active task for the seeded agent and return its id.
    async fn seed_task(&self, name: &str, schedule: ScheduleSpec) -> ScheduledTaskId {
        let next_run_at = schedule.next_after(self.clock.now_utc());
        self.sched
            .create(NewScheduledTask {
                owner_agent_id: self.seed.agent_id,
                org_id: self.seed.org_id,
                created_by_user_id: self.seed.user_id,
                channel_id: None,
                name: ScheduledTaskName::try_from(name).expect("valid name"),
                prompt: ScheduledPrompt::try_from("do the thing").expect("valid prompt"),
                schedule,
                next_run_at,
            })
            .await
            .expect("create scheduled task")
            .id
    }
}

// ─── schedule fixtures ─────────────────────────────────────────────────

fn future() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2030, 1, 1, 9, 0, 0)
        .single()
        .expect("unambiguous fixture instant")
}

fn recurring_workdays() -> ScheduleSpec {
    ScheduleSpec::Recurring {
        weekdays: Weekdays::WORKDAYS,
        time: TimeOfDay::try_new(9, 0).expect("valid time"),
        tz: Timezone::try_from("UTC").expect("valid tz"),
    }
}

fn once_future() -> ScheduleSpec {
    ScheduleSpec::Once { run_at: future() }
}

// ─── request helpers ───────────────────────────────────────────────────

async fn http_get(
    state: AppState,
    uri: &str,
    cookie: &str,
) -> (axum::http::StatusCode, serde_json::Value) {
    dispatch(state, "GET", uri, Some(cookie), None).await
}

async fn dispatch(
    state: AppState,
    method: &str,
    uri: &str,
    cookie: Option<&str>,
    csrf: Option<&str>,
) -> (axum::http::StatusCode, serde_json::Value) {
    let mut builder = axum::http::Request::builder().method(method).uri(uri);
    if let Some(c) = cookie {
        builder = builder.header("cookie", c);
    }
    if let Some(t) = csrf {
        builder = builder.header("x-csrf-token", t);
    }
    let res = router(state)
        .oneshot(
            builder
                .body(axum::body::Body::empty())
                .expect("build request"),
        )
        .await
        .expect("response");
    let status = res.status();
    let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .expect("body");
    let json = if bytes.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::from_slice(&bytes).expect("json")
    };
    (status, json)
}

/// Find the listed item whose `name` matches.
fn item<'a>(body: &'a serde_json::Value, name: &str) -> &'a serde_json::Value {
    body["items"]
        .as_array()
        .expect("items array")
        .iter()
        .find(|i| i["name"] == name)
        .unwrap_or_else(|| panic!("no item named {name}"))
}

// ─── tests ─────────────────────────────────────────────────────────────

#[sqlx::test]
async fn lists_tasks_with_summary_labels_and_states(pool: PgPool) {
    let h = Harness::new(pool).await;

    let _t1 = h.seed_task("Weekly digest", recurring_workdays()).await;
    let _t2 = h.seed_task("Migration check", once_future()).await;
    let done = h.seed_task("Old report", once_future()).await;
    let cancelled = h.seed_task("Stale reminder", recurring_workdays()).await;

    // Drive one task to `done` (→ completed) and one to `cancelled` so the
    // summary exercises all three buckets.
    h.sched
        .record_fired(done, PromptRequestId::new(), h.clock.now_utc(), None)
        .await
        .expect("mark done");
    h.sched
        .cancel(cancelled, h.seed.agent_id)
        .await
        .expect("cancel");

    let uri = format!("/api/agents/{}/scheduled-tasks", h.seed.agent_id);
    let (status, body) = http_get(h.state.clone(), &uri, &h.primary.cookie_header()).await;
    assert_eq!(status, axum::http::StatusCode::OK);

    assert_eq!(body["total"], 4);
    assert_eq!(body["summary"]["active"], 2);
    assert_eq!(body["summary"]["completed"], 1);
    assert_eq!(body["summary"]["cancelled"], 1);
    assert_eq!(body["items"].as_array().expect("items").len(), 4);

    // Active recurring: kind + cadence label + a live next-run instant.
    let weekly = item(&body, "Weekly digest");
    assert_eq!(weekly["status"], "active");
    assert_eq!(weekly["kind"], "recurring");
    assert_eq!(weekly["schedule_label"], "Every weekday, 09:00 UTC");
    assert_eq!(weekly["schedule_full"], "Every weekday at 09:00 UTC");
    assert!(weekly["next_run_label"].is_string());

    // Active one-time.
    let migration = item(&body, "Migration check");
    assert_eq!(migration["status"], "active");
    assert_eq!(migration["kind"], "one_time");
    assert!(
        migration["schedule_label"]
            .as_str()
            .expect("label")
            .starts_with("Once:")
    );

    // Completed (store `done`): no next-run, but a recorded last-run.
    let old = item(&body, "Old report");
    assert_eq!(old["status"], "completed");
    assert!(old["next_run_label"].is_null());
    assert!(old["last_run_label"].is_string());

    // Cancelled: next-run dashed out.
    let stale = item(&body, "Stale reminder");
    assert_eq!(stale["status"], "cancelled");
    assert!(stale["next_run_label"].is_null());

    // Every item carries the agent id + name the FE breadcrumb needs.
    assert_eq!(weekly["agent_id"], h.seed.agent_id.as_uuid().to_string());
    assert!(weekly["agent_name"].is_string());
}

#[sqlx::test]
async fn cancel_endpoint_flips_state_and_nulls_next_run(pool: PgPool) {
    let h = Harness::new(pool).await;
    let task = h.seed_task("Nightly sweep", recurring_workdays()).await;

    let uri = format!(
        "/api/agents/{}/scheduled-tasks/{}/cancel",
        h.seed.agent_id, task
    );
    let (status, body) = dispatch(
        h.state.clone(),
        "POST",
        &uri,
        Some(&h.primary.cookie_header()),
        Some(h.primary.csrf_header()),
    )
    .await;

    assert_eq!(status, axum::http::StatusCode::OK);
    assert_eq!(body["status"], "cancelled");
    assert_eq!(body["name"], "Nightly sweep");
    assert!(body["next_run_label"].is_null());

    // The list now reflects the cancellation in its summary.
    let list_uri = format!("/api/agents/{}/scheduled-tasks", h.seed.agent_id);
    let (_, list) = http_get(h.state.clone(), &list_uri, &h.primary.cookie_header()).await;
    assert_eq!(list["summary"]["cancelled"], 1);
    assert_eq!(list["summary"]["active"], 0);
}

#[sqlx::test]
async fn unknown_agent_404s(pool: PgPool) {
    let h = Harness::new(pool).await;
    let uri = format!("/api/agents/{}/scheduled-tasks", uuid::Uuid::nil());
    let (status, _) = http_get(h.state.clone(), &uri, &h.primary.cookie_header()).await;
    assert_eq!(status, axum::http::StatusCode::NOT_FOUND);
}

#[sqlx::test]
async fn listing_requires_a_session(pool: PgPool) {
    let h = Harness::new(pool).await;
    let uri = format!("/api/agents/{}/scheduled-tasks", h.seed.agent_id);
    let (status, _) = dispatch(h.state.clone(), "GET", &uri, None, None).await;
    assert_eq!(status, axum::http::StatusCode::UNAUTHORIZED);
}
