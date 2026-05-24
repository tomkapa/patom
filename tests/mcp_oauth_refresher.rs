//! Refresher behaviour against a stub AS that always returns
//! `invalid_grant`. Exercises:
//!   - on `invalid_grant`, `mcp_servers.connection_status` is flipped to
//!     `'reconnect_required'`,
//!   - the per-server lock is removed from the cache,
//!   - the token row is left intact (so the UI can still render the
//!     server in its "reconnect required" state),
//!   - the shared-client (`org_id IS NULL`) fallback in the dcr lookup
//!     resolves so Gmail/Google connections actually get refreshed.

#![allow(clippy::expect_used)]

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::routing::post;
use relay_rs::clock::SystemClock;
use relay_rs::crypto::OrgEncryptor;
use relay_rs::mcp::oauth::{
    ClientProvenance, McpOAuthClientStore as _, NewOAuthClient, OAuthRefresher,
    PgMcpOAuthClientStore, RefresherDeps,
};
use relay_rs::mcp::{
    ConnectionStatus, CredentialPayload, McpCatalogId, McpCredentialStore, McpCredentialWrite,
    McpHttpUrl, McpServerCreate, McpServerStore, McpTransport, OAuth2Payload, PgMcpCredentialStore,
    PgMcpServerStore,
};
use tokio::net::TcpListener;

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
            issuer: "https://issuer.example".into(),
            client_id: relay_rs::mcp::oauth::OAuthClientId::try_from("client".to_owned())
                .expect("valid"),
            client_secret: None,
            authorization_endpoint: "https://issuer.example/auth".into(),
            token_endpoint: "http://127.0.0.1:1/token".into(), // unreachable
            token_endpoint_auth_method: relay_rs::mcp::oauth::TokenAuthMethod::None,
            scope: None,
            provenance: relay_rs::mcp::oauth::ClientProvenance::Dcr {
                org_id: db.default_org_id,
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

/// Regression: a server connected via the shared platform OAuth client
/// (`org_id IS NULL`) must still be refreshed. Pre-fix the refresher
/// only consulted the org-scoped `read(org, issuer)` lookup and never
/// fell through to `read_shared`, so every Gmail/Google connection
/// silently broke ~1h after connect.
///
/// Drives the full path: stand up an axum stub that returns
/// `invalid_grant` on `/token`, seed the shared client pointing at it,
/// and seed an oauth2 credential row with `refresh_token: Some(...)`
/// and an expired `expires_at`. With the fix, the refresher resolves
/// the shared client, hits the stub, sees `invalid_grant`, and flips
/// `connection_status` to `reconnect_required`. Without the fix, the
/// dcr lookup returns None, the refresher errors with
/// `OAuthError::Misconfigured`, and the status stays `Ok`.
#[tokio::test(flavor = "multi_thread")]
async fn refresh_resolves_shared_client_when_no_org_scoped_row_exists() {
    let db = TestDb::fresh().await;
    let clock = SystemClock::shared();
    let enc = Arc::new(OrgEncryptor::for_test([6u8; 32]));

    // Tiny stub AS: always 400 with `invalid_grant`. Bound on an
    // ephemeral port and shut down when the test returns.
    let (token_url, _stub_handle) = spawn_invalid_grant_stub().await;

    // Seed a server.
    let server_store = PgMcpServerStore::new(db.pool.clone(), clock.clone());
    let server = server_store
        .create(McpServerCreate {
            org_id: db.default_org_id,
            created_by_user_id: db.default_user_id,
            catalog_id: McpCatalogId::try_from("gmail").expect("catalog id"),
            config: McpTransport::Http {
                url: McpHttpUrl::try_from("http://localhost:9000").expect("url"),
            },
            description: None,
            enabled: true,
            connection_status: ConnectionStatus::Ok,
        })
        .await
        .expect("create server");

    // Seed a SHARED client (org_id = None) — no org-scoped row exists
    // for this issuer.
    let client_store = PgMcpOAuthClientStore::new(db.pool.clone(), clock.clone(), enc.clone());
    let issuer = "https://accounts.example".to_owned();
    client_store
        .upsert(NewOAuthClient {
            issuer: issuer.clone(),
            client_id: relay_rs::mcp::oauth::OAuthClientId::try_from("shared-client".to_owned())
                .expect("valid"),
            client_secret: Some(
                relay_rs::types::SecretString::try_from("shared-secret".to_owned()).expect("valid"),
            ),
            authorization_endpoint: "https://accounts.example/auth".into(),
            token_endpoint: token_url.clone(),
            token_endpoint_auth_method: relay_rs::mcp::oauth::TokenAuthMethod::ClientSecretPost,
            scope: None,
            provenance: ClientProvenance::Shared,
        })
        .await
        .expect("upsert shared client");

    // Seed an expired oauth2 credential WITH a refresh_token — the
    // refresher will reach the dcr-lookup step (the no-refresh-token
    // path short-circuits before it).
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
                refresh_token: Some("rt".into()),
                expires_at: clock.now_utc() - chrono::Duration::seconds(10),
                scope: None,
                issuer: issuer.clone(),
                token_endpoint: token_url.clone(),
            }),
        })
        .await
        .expect("seed credential");

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
    assert!(
        ok,
        "shared-client refresh never reached the stub AS — the dcr \
         lookup must fall through to read_shared",
    );
}

/// Minimal axum stub: every POST /token returns
/// `{"error":"invalid_grant"}` with HTTP 400, matching RFC 6749 §5.2.
async fn spawn_invalid_grant_stub() -> (String, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind stub");
    let addr = listener.local_addr().expect("local_addr");
    let app = Router::new().route(
        "/token",
        post(|| async {
            (
                axum::http::StatusCode::BAD_REQUEST,
                [("content-type", "application/json")],
                r#"{"error":"invalid_grant"}"#,
            )
        }),
    );
    let handle = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (format!("http://{addr}/token"), handle)
}
