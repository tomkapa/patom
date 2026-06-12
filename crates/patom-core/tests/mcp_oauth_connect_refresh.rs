//! Reconnect/refresh path for OAuth-credentialed MCP servers.
//!
//! Regression: the connect path
//! ([`patom::mcp::oauth::build_manager_for_request`]) handed rmcp's
//! `AuthorizationManager` a credential store but never called
//! `configure_client`. While the cached access token was fresh rmcp
//! injected it without needing the client, so the bug stayed hidden — but
//! the first reconnect after the token expired hit
//! `AuthorizationManager::refresh_token`, which dereferences
//! `manager.oauth_client`, and failed with `Internal error: OAuth client
//! not configured`. Notion (≈1 h tokens) surfaced it first; every oauth2
//! vendor is affected once its token ages out.
//!
//! These tests stand up a mock Authorization Server that serves discovery
//! plus a refresh `/token` endpoint, seed an *expired* access token with a
//! refresh token, build the manager exactly as the registry's connect path
//! does, and assert `get_access_token` refreshes instead of erroring.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::sync::{Arc, Mutex};

use base64::Engine as _;
use chrono::Utc;
use oauth2::basic::BasicTokenType;
use oauth2::{AccessToken, RefreshToken};
use patom::clock::SystemClock;
use patom::config::PlatformOAuthClient;
use patom::crypto::OrgEncryptor;
use patom::mcp::oauth::{ConnectCtx, build_manager_for_request};
use patom::mcp::{
    ClientSource, ConnectionStatus, CredentialPayload, McpAuthKind, McpCatalogDescription,
    McpCatalogDisplayName, McpCatalogEntry, McpCatalogId, McpCredentialWrite, McpHttpUrl,
    McpServerCreate, McpServerStore, McpTransport, OAuth2Payload, PgMcpCredentialStore,
    PgMcpServerStore, SharedMcpCredentialStore, platform_env_middle,
};
use patom::types::SecretString;
use rmcp::transport::auth::{OAuthTokenResponse, StoredCredentials, VendorExtraTokenFields};
use sqlx::PgPool;
use std::collections::HashMap;
use std::time::Duration;

mod common;
use common::pg::seed_tenant;

/// Access token the mock AS hands back from its refresh `/token` endpoint.
const REFRESHED_ACCESS_TOKEN: &str = "refreshed-access-token-xyz";

/// Spawn a throwaway Authorization Server that answers rmcp's discovery
/// probe and a refresh-grant `/token` endpoint. Captured request bodies for
/// `/token` are pushed into `seen_bodies` so a caller can assert what client
/// credentials were sent. Returns the AS base URL.
async fn spawn_refresh_authorization_server(seen_bodies: Arc<Mutex<Vec<String>>>) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock AS");
    let base = format!("http://{}", listener.local_addr().expect("addr"));
    let meta_base = base.clone();
    let app = axum::Router::new()
        .route(
            "/.well-known/oauth-authorization-server",
            axum::routing::get(move || {
                let b = meta_base.clone();
                async move {
                    axum::Json(serde_json::json!({
                        "issuer": b,
                        "authorization_endpoint": format!("{b}/authorize"),
                        "token_endpoint": format!("{b}/token"),
                        "registration_endpoint": format!("{b}/register"),
                        "response_types_supported": ["code"],
                        "code_challenge_methods_supported": ["S256"],
                        "scopes_supported": ["read"],
                    }))
                }
            }),
        )
        .route(
            "/token",
            axum::routing::post(move |headers: axum::http::HeaderMap, body: String| {
                let seen = seen_bodies.clone();
                async move {
                    // Record the form body plus the `Authorization`
                    // header — oauth2 sends a confidential client's
                    // secret via HTTP Basic auth, not the form body, so
                    // both are needed to assert what was forwarded.
                    let auth = headers
                        .get(axum::http::header::AUTHORIZATION)
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("")
                        .to_owned();
                    seen.lock().expect("lock").push(format!("{auth}\n{body}"));
                    axum::Json(serde_json::json!({
                        "access_token": REFRESHED_ACCESS_TOKEN,
                        "token_type": "Bearer",
                        "expires_in": 3600,
                        "refresh_token": "rotated-refresh-token",
                    }))
                }
            }),
        );
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve mock AS");
    });
    base
}

/// Create a server row and seed an *expired* OAuth credential (valid
/// refresh token) for it. Returns the server id.
async fn seed_expired_oauth_server(
    pool: &PgPool,
    seed: &common::pg::Seed,
    creds: &SharedMcpCredentialStore,
    catalog_id: &str,
    server_url: &str,
    stored_client_id: &str,
) -> patom::mcp::McpServerId {
    let store = PgMcpServerStore::new(pool.clone(), SystemClock::shared());
    let record = store
        .create(McpServerCreate {
            org_id: seed.org_id,
            created_by_user_id: seed.user_id,
            catalog_id: McpCatalogId::try_from(catalog_id).expect("catalog id"),
            config: McpTransport::Http {
                url: McpHttpUrl::try_from(server_url).expect("url"),
            },
            description: None,
            enabled: true,
            connection_status: ConnectionStatus::Ok,
        })
        .await
        .expect("create server");

    let mut token = OAuthTokenResponse::new(
        AccessToken::new("stale-access-token".to_owned()),
        BasicTokenType::Bearer,
        VendorExtraTokenFields::default(),
    );
    // expires_in tiny + token_received_at far in the past → rmcp computes a
    // remaining lifetime of 0 and takes the refresh branch.
    let lifetime = Duration::from_secs(30);
    token.set_expires_in(Some(&lifetime));
    token.set_refresh_token(Some(RefreshToken::new("seed-refresh-token".to_owned())));
    let stored = StoredCredentials::new(
        stored_client_id.to_owned(),
        Some(token),
        vec!["read".to_owned()],
        Some(1_700_000_000),
    );
    creds
        .upsert(McpCredentialWrite {
            server_id: record.id,
            org_id: seed.org_id,
            payload: CredentialPayload::Oauth2(OAuth2Payload::new(stored)),
        })
        .await
        .expect("upsert credential");
    record.id
}

/// Build an in-memory catalog entry pointed at `server_url`. Timestamps are
/// cosmetic for the connect path.
fn catalog_entry(
    catalog_id: &str,
    server_url: &str,
    client_source: ClientSource,
) -> McpCatalogEntry {
    let now = Utc::now();
    McpCatalogEntry {
        id: McpCatalogId::try_from(catalog_id).expect("catalog id"),
        org_id: None,
        display_name: McpCatalogDisplayName::try_from("Mock").expect("name"),
        description: McpCatalogDescription::try_from("Mock OAuth connector").expect("desc"),
        homepage_url: None,
        icon_url: None,
        default_transport: McpTransport::Http {
            url: McpHttpUrl::try_from(server_url).expect("url"),
        },
        auth_kind: McpAuthKind::OAuth2,
        default_scope: None,
        authorize_extra_params: None,
        client_source,
        platform_client_alias: None,
        created_at: now,
        updated_at: now,
    }
}

#[sqlx::test]
async fn dcr_server_refreshes_expired_token_on_connect(pool: PgPool) {
    // The reported Notion failure: a DCR connector whose access token has
    // expired must refresh on reconnect, not error "OAuth client not
    // configured". The DCR client is public — its id rides in the persisted
    // `StoredCredentials`.
    let seed = seed_tenant(&pool).await;
    let creds: SharedMcpCredentialStore = Arc::new(PgMcpCredentialStore::new(
        pool.clone(),
        SystemClock::shared(),
        Arc::new(OrgEncryptor::for_test([3u8; 32])),
    ));
    let seen_bodies = Arc::new(Mutex::new(Vec::new()));
    let as_base = spawn_refresh_authorization_server(seen_bodies.clone()).await;

    let server_id = seed_expired_oauth_server(
        &pool,
        &seed,
        &creds,
        "notion",
        &as_base,
        "dcr-public-client-1",
    )
    .await;

    let catalog = catalog_entry("notion", &as_base, ClientSource::Dcr);
    let platform_clients = HashMap::new();
    let manager = build_manager_for_request(ConnectCtx {
        catalog: &catalog,
        base_url: &as_base,
        http: reqwest::Client::new(),
        platform_clients: &platform_clients,
        user_oauth_client: None,
        credentials: creds.clone(),
        server_id,
        org_id: seed.org_id,
    })
    .await
    .expect("build manager");

    let token = manager
        .get_access_token()
        .await
        .expect("refresh should succeed, not 'OAuth client not configured'");
    assert_eq!(token, REFRESHED_ACCESS_TOKEN);

    // The refresh actually hit the AS, and as a public DCR client it sent
    // its client_id but no client_secret.
    let bodies = seen_bodies.lock().expect("lock");
    assert_eq!(bodies.len(), 1, "exactly one refresh request");
    assert!(
        bodies[0].contains("grant_type=refresh_token"),
        "refresh grant, got: {}",
        bodies[0]
    );
    assert!(
        !bodies[0].contains("client_secret"),
        "public DCR client must not send a secret, got: {}",
        bodies[0]
    );
}

#[sqlx::test]
async fn platform_server_refresh_forwards_client_secret(pool: PgPool) {
    // Solution-2 coverage: a confidential Platform client must re-apply its
    // client_secret on reconnect (rmcp's `configure_client_id` shortcut omits
    // it), so the refresh request carries the secret the AS requires.
    let seed = seed_tenant(&pool).await;
    let creds: SharedMcpCredentialStore = Arc::new(PgMcpCredentialStore::new(
        pool.clone(),
        SystemClock::shared(),
        Arc::new(OrgEncryptor::for_test([4u8; 32])),
    ));
    let seen_bodies = Arc::new(Mutex::new(Vec::new()));
    let as_base = spawn_refresh_authorization_server(seen_bodies.clone()).await;

    let catalog_id = "linear";
    let server_id = seed_expired_oauth_server(
        &pool,
        &seed,
        &creds,
        catalog_id,
        &as_base,
        "platform-client-from-env",
    )
    .await;

    let catalog = catalog_entry(catalog_id, &as_base, ClientSource::Platform);
    let mut platform_clients = HashMap::new();
    platform_clients.insert(
        platform_env_middle(&catalog.id),
        PlatformOAuthClient {
            client_id: SecretString::try_from("platform-client-from-env".to_owned())
                .expect("client id"),
            client_secret: SecretString::try_from("platform-shared-secret".to_owned())
                .expect("client secret"),
        },
    );

    let manager = build_manager_for_request(ConnectCtx {
        catalog: &catalog,
        base_url: &as_base,
        http: reqwest::Client::new(),
        platform_clients: &platform_clients,
        user_oauth_client: None,
        credentials: creds.clone(),
        server_id,
        org_id: seed.org_id,
    })
    .await
    .expect("build manager");

    let token = manager
        .get_access_token()
        .await
        .expect("refresh should succeed");
    assert_eq!(token, REFRESHED_ACCESS_TOKEN);

    // oauth2 sends a confidential client's id+secret as HTTP Basic auth on
    // the refresh request. Decode it and assert the platform secret rode
    // along — that only happens if `configure_oauth_client` re-applied it.
    let bodies = seen_bodies.lock().expect("lock");
    assert_eq!(bodies.len(), 1, "exactly one refresh request");
    let auth_line = bodies[0].lines().next().unwrap_or("");
    let b64 = auth_line
        .strip_prefix("Basic ")
        .expect("confidential client must use HTTP Basic auth");
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(b64)
        .expect("valid base64 basic credentials");
    let creds = String::from_utf8(decoded).expect("utf8 credentials");
    assert_eq!(creds, "platform-client-from-env:platform-shared-secret");
}
