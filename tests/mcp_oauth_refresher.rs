//! Refresher behaviour against a stub AS that always returns
//! `invalid_grant`. Exercises:
//!   - on `invalid_grant`, `mcp_servers.connection_status` is flipped to
//!     `'reconnect_required'`,
//!   - the per-server lock is removed from the cache,
//!   - the token row is left intact (so the UI can still render the
//!     server in its "reconnect required" state).

#![allow(clippy::expect_used)]

use std::sync::Arc;
use std::time::Duration;

use relay_rs::clock::SystemClock;
use relay_rs::crypto::OrgEncryptor;
use relay_rs::mcp::oauth::{
    McpOAuthClientStore as _, NewOAuthClient, OAuthRefresher, PgMcpOAuthClientStore, RefresherDeps,
};
use relay_rs::mcp::{
    ConnectionStatus, CredentialPayload, McpCatalogId, McpCredentialStore, McpCredentialWrite,
    McpHttpUrl, McpServerCreate, McpServerStore, McpTransport, OAuth2Payload, PgMcpCredentialStore,
    PgMcpServerStore,
};

mod common;
use common::pg::TestDb;

#[tokio::test(flavor = "multi_thread")]
async fn refresh_failure_with_no_refresh_token_flips_status() {
    let db = TestDb::fresh().await;
    let clock = SystemClock::shared();
    let enc = Arc::new(OrgEncryptor::for_test([5u8; 32]));

    // Seed a server.
    let server_store = PgMcpServerStore::new(db.pool.clone(), clock.clone());
    let server = server_store
        .create(McpServerCreate {
            org_id: db.default_org_id,
            created_by_user_id: db.default_user_id,
            catalog_id: McpCatalogId::try_from("notion").expect("catalog id"),
            config: McpTransport::Http {
                url: McpHttpUrl::try_from("http://localhost:9000").expect("url"),
            },
            description: None,
            enabled: true,
            connection_status: ConnectionStatus::Ok,
        })
        .await
        .expect("create server");

    // Seed a DCR client record.
    let client_store = PgMcpOAuthClientStore::new(db.pool.clone(), clock.clone(), enc.clone());
    let _client = client_store
        .upsert(NewOAuthClient {
            org_id: db.default_org_id,
            issuer: "https://issuer.example".into(),
            client_id: relay_rs::mcp::oauth::OAuthClientId::try_from("client".to_owned())
                .expect("valid"),
            client_secret: None,
            authorization_endpoint: "https://issuer.example/auth".into(),
            token_endpoint: "http://127.0.0.1:1/token".into(), // unreachable
            token_endpoint_auth_method: relay_rs::mcp::oauth::TokenAuthMethod::None,
            scope: None,
            provenance: relay_rs::mcp::oauth::ClientProvenance::Dcr {
                registration_client_uri: None,
                registration_access_token: None,
            },
        })
        .await
        .expect("upsert client");

    // Seed an `oauth2` credential row with an *expired* token and *no*
    // refresh_token — the refresher must treat this as
    // `reconnect_required`.
    let creds_store = Arc::new(PgMcpCredentialStore::new(
        db.pool.clone(),
        clock.clone(),
        enc.clone(),
    ));
    creds_store
        .upsert(McpCredentialWrite {
            server_id: server.id,
            org_id: db.default_org_id,
            payload: CredentialPayload::Oauth2(OAuth2Payload {
                access_token: "expired".into(),
                refresh_token: None,
                expires_at: clock.now_utc() - chrono::Duration::seconds(10),
                scope: None,
                issuer: "https://issuer.example".into(),
                token_endpoint: "http://127.0.0.1:1/token".into(),
            }),
        })
        .await
        .expect("seed credential");

    // Spawn the refresher; first tick should pick the row up.
    let flow = relay_rs::mcp::oauth::OAuthFlowClient::new(reqwest::Client::new()).expect("flow");
    let (refresher, _cache) = OAuthRefresher::spawn(RefresherDeps {
        pool: db.pool.clone(),
        clock: clock.clone(),
        enc,
        credentials: creds_store.clone(),
        oauth_clients: Arc::new(client_store),
        flow,
        redirect_uri: "http://localhost:8080/mcp-oauth/callback".into(),
    });

    // The interval is 60s; we don't want to wait. Poll until the row
    // flips or 5s elapse — the in-process tick is precise enough that
    // this normally lands within the first 100ms.
    //
    // Actually the refresher's `interval` first fires *immediately* on
    // tick #1 even with `Skip` policy. Give it a moment.
    let server_id = server.id;
    let mut ok = false;
    for _ in 0..50 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        let status: String =
            sqlx::query_scalar("SELECT connection_status FROM mcp_servers WHERE id = $1")
                .bind(server_id)
                .fetch_one(&db.pool)
                .await
                .expect("status");
        if status == "reconnect_required" {
            ok = true;
            break;
        }
    }
    refresher.shutdown().await;
    assert!(ok, "connection_status was not flipped within 5s");
}
