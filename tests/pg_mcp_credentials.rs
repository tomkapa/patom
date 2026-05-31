//! Roundtrip tests for the encrypted `mcp_server_credentials` envelope
//! after the rmcp-auth adoption.
//!
//! Patom's `OAuth2Payload` wraps `rmcp::transport::auth::StoredCredentials`
//! verbatim. The seal/open path must round-trip the inner struct without
//! lossy field translation (rmcp's `client_id`, `token_response`,
//! `granted_scopes`, `token_received_at` all survive).

#![allow(clippy::expect_used)]

use std::sync::Arc;

use oauth2::AccessToken;
use oauth2::RefreshToken;
use oauth2::TokenResponse as _;
use oauth2::basic::BasicTokenType;
use patom_rs::clock::SystemClock;
use patom_rs::crypto::OrgEncryptor;
use patom_rs::mcp::{
    ConnectionStatus, CredentialPayload, McpCatalogId, McpCredentialStore, McpCredentialWrite,
    McpHttpUrl, McpServerCreate, McpServerStore, McpTransport, OAuth2Payload, PgMcpCredentialStore,
    PgMcpServerStore,
};
use rmcp::transport::auth::{OAuthTokenResponse, StoredCredentials, VendorExtraTokenFields};
use sqlx::PgPool;

mod common;
use common::pg::seed_tenant;

async fn seed_server(pool: &PgPool, seed: &common::pg::Seed) -> patom_rs::mcp::McpServerId {
    let clock = SystemClock::shared();
    let store = PgMcpServerStore::new(pool.clone(), clock);
    let record = store
        .create(McpServerCreate {
            org_id: seed.org_id,
            created_by_user_id: seed.user_id,
            catalog_id: McpCatalogId::try_from("notion").expect("catalog id"),
            config: McpTransport::Http {
                url: McpHttpUrl::try_from("https://example.test/mcp").expect("url"),
            },
            description: None,
            enabled: true,
            connection_status: ConnectionStatus::Ok,
        })
        .await
        .expect("create server");
    record.id
}

#[sqlx::test]
async fn oauth2_payload_roundtrips_full_stored_credentials(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let server_id = seed_server(&pool, &seed).await;
    let clock = SystemClock::shared();
    let enc = Arc::new(OrgEncryptor::for_test([7u8; 32]));
    let store: Arc<dyn McpCredentialStore> =
        Arc::new(PgMcpCredentialStore::new(pool.clone(), clock.clone(), enc));

    let mut token = OAuthTokenResponse::new(
        AccessToken::new("at-123".to_owned()),
        BasicTokenType::Bearer,
        VendorExtraTokenFields::default(),
    );
    token.set_refresh_token(Some(RefreshToken::new("rt-456".to_owned())));
    let stored = StoredCredentials::new(
        "dcr-client-789".to_owned(),
        Some(token),
        vec!["read".to_owned(), "write".to_owned()],
        Some(1_700_000_000),
    );

    store
        .upsert(McpCredentialWrite {
            server_id,
            org_id: seed.org_id,
            payload: CredentialPayload::Oauth2(OAuth2Payload::new(stored.clone())),
        })
        .await
        .expect("upsert");

    let record = store
        .read(server_id, seed.org_id)
        .await
        .expect("read")
        .expect("row present");
    let CredentialPayload::Oauth2(got) = record.payload else {
        panic!("variant mismatch");
    };
    let s = got.as_stored();
    assert_eq!(s.client_id, "dcr-client-789");
    assert_eq!(s.granted_scopes, vec!["read", "write"]);
    assert_eq!(s.token_received_at, Some(1_700_000_000));
    let token_back = s.token_response.as_ref().expect("token_response preserved");
    assert_eq!(token_back.access_token().secret(), "at-123");
    assert_eq!(
        token_back.refresh_token().map(|t| t.secret().as_str()),
        Some("rt-456"),
    );
}

#[sqlx::test]
async fn oauth2_payload_without_refresh_token_roundtrips(pool: PgPool) {
    // Some Platform-credential entries (vendors that don't issue
    // refresh tokens) leave that slot `None`; confirm the absence
    // survives a seal+open round-trip.
    let seed = seed_tenant(&pool).await;
    let server_id = seed_server(&pool, &seed).await;
    let clock = SystemClock::shared();
    let enc = Arc::new(OrgEncryptor::for_test([9u8; 32]));
    let store: Arc<dyn McpCredentialStore> =
        Arc::new(PgMcpCredentialStore::new(pool.clone(), clock.clone(), enc));

    let token = OAuthTokenResponse::new(
        AccessToken::new("at-platform".to_owned()),
        BasicTokenType::Bearer,
        VendorExtraTokenFields::default(),
    );
    let stored = StoredCredentials::new(
        "platform-client".to_owned(),
        Some(token),
        Vec::new(),
        Some(1_700_000_000),
    );

    store
        .upsert(McpCredentialWrite {
            server_id,
            org_id: seed.org_id,
            payload: CredentialPayload::Oauth2(OAuth2Payload::new(stored)),
        })
        .await
        .expect("upsert");

    let CredentialPayload::Oauth2(got) = store
        .read(server_id, seed.org_id)
        .await
        .expect("read")
        .expect("row present")
        .payload
    else {
        panic!("variant mismatch");
    };
    let token_back = got
        .into_stored()
        .token_response
        .expect("token_response preserved");
    assert_eq!(token_back.access_token().secret(), "at-platform");
    assert!(token_back.refresh_token().is_none());
}
