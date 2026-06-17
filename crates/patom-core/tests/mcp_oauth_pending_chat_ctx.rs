//! Round-trip coverage for the Lark / Discord ping-context columns added to
//! `mcp_oauth_pending` (migration 86). Drives the real
//! [`PgMcpOAuthPendingStore`] `save` → `read_pending_ctx` path over Postgres
//! so the new SQL columns + all-or-none decode are exercised end-to-end —
//! the seam the OAuth callback reads to fire `do_lark_ping` / `do_discord_ping`
//! and the universal auto-continue.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::sync::Arc;

use oauth2::{CsrfToken, PkceCodeVerifier};
use patom::agents::AgentId;
use patom::clock::SystemClock;
use patom::mcp::oauth::{
    DiscordPingCtx, LarkPingCtx, PatomPendingCtx, PgMcpOAuthPendingStore, ResumeCtx,
};
use patom::mcp::{
    ConnectionStatus, McpCatalogId, McpHttpUrl, McpServerCreate, McpServerId, McpServerStore,
    McpTransport, PgMcpServerStore,
};
use patom::threads::ThreadId;
use rmcp::transport::auth::{StateStore, StoredAuthorizationState};
use sqlx::PgPool;

mod common;
use common::pg::{Seed, seed_tenant};

/// Create a `notion` MCP server row so the pending FK (`server_id`) resolves.
async fn seed_server(pool: &PgPool, seed: &Seed) -> McpServerId {
    let store = PgMcpServerStore::new(pool.clone(), SystemClock::shared());
    store
        .create(McpServerCreate {
            org_id: seed.org_id,
            created_by_user_id: seed.user_id,
            catalog_id: McpCatalogId::try_from("notion").expect("catalog id"),
            config: McpTransport::Http {
                url: McpHttpUrl::try_from("http://localhost:9000/").expect("url"),
            },
            description: None,
            enabled: true,
            connection_status: ConnectionStatus::AuthPending,
        })
        .await
        .expect("create server")
        .id
}

/// Persist `ctx` under a fresh state token via the writer, then read it back.
async fn round_trip(pool: &PgPool, ctx: PatomPendingCtx, state_token: &str) -> PatomPendingCtx {
    let store = Arc::new(PgMcpOAuthPendingStore::new(
        pool.clone(),
        SystemClock::shared(),
    ));
    let stored = StoredAuthorizationState::new(
        &PkceCodeVerifier::new("v".repeat(48)),
        &CsrfToken::new(state_token.to_owned()),
    );
    store
        .clone()
        .writer(ctx)
        .save(state_token, stored)
        .await
        .expect("save pending");
    store
        .read_pending_ctx(state_token)
        .await
        .expect("read pending")
        .expect("pending present")
}

fn base_ctx(seed: &Seed, server_id: McpServerId) -> PatomPendingCtx {
    PatomPendingCtx {
        server_id,
        user_id: seed.user_id,
        org_id: seed.org_id,
        redirect_to: None,
        resume_ctx: Some(ResumeCtx {
            thread_id: ThreadId::new(),
            agent_id: AgentId::new(),
        }),
        slack_ctx: None,
        lark_ctx: None,
        discord_ctx: None,
        expires_at: chrono::Utc::now() + chrono::Duration::minutes(10),
    }
}

#[sqlx::test]
async fn lark_ctx_round_trips_through_store(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let server_id = seed_server(&pool, &seed).await;
    let lark_ctx = LarkPingCtx {
        app_id: "cli_app".to_owned(),
        chat_id: "oc_chat".to_owned(),
        reply_to: Some("om_msg".to_owned()),
    };
    let ctx = PatomPendingCtx {
        lark_ctx: Some(lark_ctx.clone()),
        ..base_ctx(&seed, server_id)
    };
    let resume = ctx.resume_ctx;
    let read = round_trip(&pool, ctx, &"a".repeat(40)).await;
    assert_eq!(read.lark_ctx, Some(lark_ctx));
    assert_eq!(
        read.resume_ctx, resume,
        "resume_ctx survives alongside lark_ctx"
    );
    assert!(read.discord_ctx.is_none());
    assert!(read.slack_ctx.is_none());
}

#[sqlx::test]
async fn lark_ctx_round_trips_without_reply_to(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let server_id = seed_server(&pool, &seed).await;
    let ctx = PatomPendingCtx {
        lark_ctx: Some(LarkPingCtx {
            app_id: "cli_app".to_owned(),
            chat_id: "oc_chat".to_owned(),
            reply_to: None,
        }),
        ..base_ctx(&seed, server_id)
    };
    let read = round_trip(&pool, ctx, &"b".repeat(40)).await;
    assert_eq!(
        read.lark_ctx,
        Some(LarkPingCtx {
            app_id: "cli_app".to_owned(),
            chat_id: "oc_chat".to_owned(),
            reply_to: None,
        })
    );
}

#[sqlx::test]
async fn discord_ctx_round_trips_through_store(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let server_id = seed_server(&pool, &seed).await;
    let discord_ctx = DiscordPingCtx {
        application_id: "123456".to_owned(),
        container_id: "789012".to_owned(),
        reply_to: Some("345678".to_owned()),
    };
    let ctx = PatomPendingCtx {
        discord_ctx: Some(discord_ctx.clone()),
        ..base_ctx(&seed, server_id)
    };
    let read = round_trip(&pool, ctx, &"c".repeat(40)).await;
    assert_eq!(read.discord_ctx, Some(discord_ctx));
    assert!(read.lark_ctx.is_none());
    assert!(read.slack_ctx.is_none());
}
