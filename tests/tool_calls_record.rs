//! Integration tests for [`PgToolCallStore`]. The store is the single
//! writer to the `tool_calls` table; the agent dispatcher invokes it after
//! every tool result.
//!
//! What's exercised here:
//!   - generic insert (no MCP server) — future non-MCP writers
//!   - MCP-tagged insert (with `mcp_server_id`)
//!   - `tool_calls_enforce_org` trigger rejects org mismatch
//!   - RLS hides rows from a non-member principal
//!
//! Driving from the full agent + worker would couple this to a stub MCP
//! server; the store-level surface is the right unit because the
//! dispatcher's job is just to build a `ToolCallRow` and call `record`.

use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use patom::auth::{OrgId, UserId};
use patom::clock::SystemClock;
use patom::mcp::McpServerId;
use patom::session::{PgSessionStore, SharedSessionStore};
use patom::tools::{
    MAX_TOOL_CALL_ERROR_MESSAGE_BYTES, PgToolCallStore, ToolCallRow, ToolCallRowId, ToolCallStore,
    clip_error_message,
};
use patom::types::ToolName;

mod common;
use common::pg::{human_to_agent_session, seed_prompt_request, seed_tenant};
use sqlx::PgPool;

fn fresh_row(
    org_id: OrgId,
    session_id: patom::session::SessionId,
    request_id: patom::runtime::PromptRequestId,
    agent_id: patom::agents::AgentId,
    mcp_server_id: Option<McpServerId>,
    is_error: bool,
) -> ToolCallRow {
    let error_message = is_error.then(|| "boom".to_owned());
    ToolCallRow {
        id: ToolCallRowId::new(),
        org_id,
        session_id,
        request_id,
        agent_id,
        mcp_server_id,
        tool_name: ToolName::try_from("web_fetch").expect("valid name"),
        started_at: Utc::now(),
        duration: Duration::from_millis(42),
        is_error,
        error_message,
    }
}

async fn seed_mcp_server(pool: &PgPool, seed: &common::pg::Seed) -> McpServerId {
    let id = McpServerId::new();
    let now = Utc::now();
    sqlx::query(
        "INSERT INTO mcp_servers
             (id, org_id, catalog_id, enabled, config, description,
              last_seen_at, last_error, discovered_tools,
              created_at, updated_at, created_by_user_id)
         VALUES ($1, $2, 'notion', TRUE,
                 '{\"transport\":\"http\",\"url\":\"http://example/mcp\"}'::jsonb,
                 NULL, NULL, NULL, NULL, $3, $3, $4)",
    )
    .bind(id)
    .bind(seed.org_id)
    .bind(now)
    .bind(seed.user_id)
    .execute(pool)
    .await
    .expect("seed mcp server");
    id
}

async fn count_rows(pool: &sqlx::PgPool) -> i64 {
    sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM tool_calls")
        .fetch_one(pool)
        .await
        .expect("count")
}

/// Seed a fresh human↔agent session and a stub `prompt_requests` row for
/// it. Every test in this file starts from this pair before exercising
/// the recorder.
async fn setup_session_and_request(
    pool: &PgPool,
    seed: &common::pg::Seed,
) -> (
    patom::session::SessionId,
    patom::runtime::PromptRequestId,
) {
    let sessions: SharedSessionStore =
        Arc::new(PgSessionStore::new(pool.clone(), SystemClock::shared()));
    let session =
        human_to_agent_session(sessions.as_ref(), seed.agent_id, seed.org_id, seed.user_id).await;
    let request_id = seed_prompt_request(pool, session, seed.agent_id, seed.org_id).await;
    (session, request_id)
}

#[sqlx::test]
async fn records_generic_row_without_mcp_server(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let (session, request_id) = setup_session_and_request(&pool, &seed).await;

    let store = PgToolCallStore::new(pool.clone(), SystemClock::shared());
    let row = fresh_row(seed.org_id, session, request_id, seed.agent_id, None, false);
    store.record(row).await.expect("record");

    let count = count_rows(&pool).await;
    assert_eq!(count, 1);

    let stored: (Option<McpServerId>, String, bool, i32) =
        sqlx::query_as("SELECT mcp_server_id, tool_name, is_error, duration_ms FROM tool_calls")
            .fetch_one(&pool)
            .await
            .expect("read back");
    assert_eq!(stored.0, None);
    assert_eq!(stored.1, "web_fetch");
    assert!(!stored.2);
    assert_eq!(stored.3, 42);
}

#[sqlx::test]
async fn records_mcp_tagged_row_indexable_by_server_and_agent(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let (session, request_id) = setup_session_and_request(&pool, &seed).await;
    let mcp = seed_mcp_server(&pool, &seed).await;

    let store = PgToolCallStore::new(pool.clone(), SystemClock::shared());
    let mut row = fresh_row(
        seed.org_id,
        session,
        request_id,
        seed.agent_id,
        Some(mcp),
        true,
    );
    row.tool_name = ToolName::try_from("mcp_probe_get").expect("valid");
    store.record(row).await.expect("record");

    // The partial indexes should be reachable via a query that mentions
    // `mcp_server_id IS NOT NULL`. Smoke-check by reading back via the
    // dashboard pattern (calls per connection).
    let per_connection: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM tool_calls
         WHERE mcp_server_id = $1
           AND mcp_server_id IS NOT NULL",
    )
    .bind(mcp)
    .fetch_one(&pool)
    .await
    .expect("per connection");
    assert_eq!(per_connection, 1);

    let per_agent: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM tool_calls
         WHERE agent_id = $1
           AND mcp_server_id IS NOT NULL",
    )
    .bind(seed.agent_id)
    .fetch_one(&pool)
    .await
    .expect("per agent");
    assert_eq!(per_agent, 1);
}

#[sqlx::test]
async fn org_trigger_rejects_mismatched_org_id(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let (session, request_id) = setup_session_and_request(&pool, &seed).await;

    let store = PgToolCallStore::new(pool.clone(), SystemClock::shared());
    let foreign_org = OrgId::new();
    let row = fresh_row(foreign_org, session, request_id, seed.agent_id, None, false);

    let err = store.record(row).await.expect_err("trigger rejects");
    let msg = format!("{err}");
    assert!(
        msg.contains("does not match parent session"),
        "unexpected error: {msg}",
    );
}

#[sqlx::test]
async fn rls_hides_rows_from_non_member_principal(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let (session, request_id) = setup_session_and_request(&pool, &seed).await;

    let store = PgToolCallStore::new(pool.clone(), SystemClock::shared());
    let row = fresh_row(seed.org_id, session, request_id, seed.agent_id, None, false);
    store.record(row).await.expect("record");

    // A user that is not a member of `default_org_id` sees zero rows.
    // The outsider must exist in `users` because `app.user_id` is set to
    // their id inside `run_as_user`; membership is what RLS checks.
    let outsider: UserId = UserId::new();
    sqlx::query(
        "INSERT INTO users (id, email, created_at, updated_at)
         VALUES ($1, $2, now(), now())",
    )
    .bind(outsider)
    .bind(format!("outsider-{outsider}@example.invalid"))
    .execute(&pool)
    .await
    .expect("seed outsider user");

    let count = patom::auth::run_as_user::<i64, sqlx::Error>(&pool, outsider, async |tx| {
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM tool_calls")
            .fetch_one(&mut **tx)
            .await?;
        Ok(n)
    })
    .await
    .expect("read as outsider");
    assert_eq!(count, 0, "RLS must hide rows from a non-member principal");

    // The owning principal still sees the row.
    let count = patom::auth::run_as_user::<i64, sqlx::Error>(&pool, seed.user_id, async |tx| {
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM tool_calls")
            .fetch_one(&mut **tx)
            .await?;
        Ok(n)
    })
    .await
    .expect("read as owner");
    assert_eq!(count, 1);
}

#[sqlx::test]
async fn records_error_row_with_message(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let (session, request_id) = setup_session_and_request(&pool, &seed).await;
    let mcp = seed_mcp_server(&pool, &seed).await;

    let store = PgToolCallStore::new(pool.clone(), SystemClock::shared());
    let row = fresh_row(
        seed.org_id,
        session,
        request_id,
        seed.agent_id,
        Some(mcp),
        true,
    );
    store.record(row).await.expect("record");

    let stored: (bool, Option<String>) =
        sqlx::query_as("SELECT is_error, error_message FROM tool_calls")
            .fetch_one(&pool)
            .await
            .expect("read back");
    assert!(stored.0);
    assert_eq!(stored.1.as_deref(), Some("boom"));
}

#[sqlx::test]
async fn db_check_rejects_error_message_on_successful_row(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let (session, request_id) = setup_session_and_request(&pool, &seed).await;

    // Bypass the recorder (which has its own assert!) and hit the DB CHECK
    // directly so we're sure the migration enforces the invariant.
    let id = ToolCallRowId::new();
    let now = Utc::now();
    let err = sqlx::query(
        "INSERT INTO tool_calls
             (id, org_id, session_id, request_id, agent_id,
              mcp_server_id, tool_name, started_at, duration_ms,
              is_error, error_message, created_at)
         VALUES ($1, $2, $3, $4, $5, NULL, 'web_fetch', $6, 1,
                 FALSE, 'should be rejected', $6)",
    )
    .bind(id)
    .bind(seed.org_id)
    .bind(session)
    .bind(request_id)
    .bind(seed.agent_id)
    .bind(now)
    .execute(&pool)
    .await
    .expect_err("CHECK rejects error_message on success row");
    let msg = format!("{err}");
    assert!(
        msg.contains("tool_calls_error_message_only_on_error"),
        "unexpected error: {msg}",
    );
}

#[test]
fn clip_error_message_passes_through_short() {
    assert_eq!(clip_error_message("nope".to_owned()), "nope");
}

#[test]
fn clip_error_message_truncates_at_byte_cap() {
    let oversize = "x".repeat(MAX_TOOL_CALL_ERROR_MESSAGE_BYTES * 2);
    let clipped = clip_error_message(oversize);
    assert!(clipped.len() <= MAX_TOOL_CALL_ERROR_MESSAGE_BYTES);
    assert_eq!(clipped.len(), MAX_TOOL_CALL_ERROR_MESSAGE_BYTES);
}

#[test]
fn clip_error_message_respects_utf8_boundary() {
    // "é" is 2 bytes. Build a string that lands the naive cut mid-codepoint
    // so the boundary walk-back is exercised.
    let prefix_len = MAX_TOOL_CALL_ERROR_MESSAGE_BYTES - 1;
    let mut s = "x".repeat(prefix_len);
    s.push('é');
    s.push_str(&"x".repeat(MAX_TOOL_CALL_ERROR_MESSAGE_BYTES));
    let clipped = clip_error_message(s);
    assert!(clipped.is_char_boundary(clipped.len()));
    assert!(clipped.len() <= MAX_TOOL_CALL_ERROR_MESSAGE_BYTES);
}
