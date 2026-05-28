//! Roundtrip tests for the encrypted `mcp_server_credentials` envelope —
//! specifically the OAuth2 payload variant after the DCR-client merge.
//!
//! Background: pre-refactor, DCR-issued client credentials lived in a
//! separate `mcp_oauth_clients` table. Migration 50 drops that table and
//! folds the client material into `OAuth2Payload` so one encrypted row
//! per server holds the access token + refresh token + (optionally) the
//! DCR client_id / client_secret / token_endpoint_auth_method.

#![allow(clippy::expect_used)]

use std::sync::Arc;

use chrono::Duration;
use patom_rs::clock::SystemClock;
use patom_rs::crypto::OrgEncryptor;
use patom_rs::mcp::{
    ConnectionStatus, CredentialPayload, McpCatalogId, McpCredentialStore, McpCredentialWrite,
    McpHttpUrl, McpServerCreate, McpServerStore, McpTransport, OAuth2Payload, PgMcpCredentialStore,
    PgMcpServerStore, TokenAuthMethod,
};

mod common;
use common::pg::TestDb;

async fn seed_server(db: &TestDb) -> patom_rs::mcp::McpServerId {
    let clock = SystemClock::shared();
    let store = PgMcpServerStore::new(db.pool.clone(), clock);
    let record = store
        .create(McpServerCreate {
            org_id: db.default_org_id,
            created_by_user_id: db.default_user_id,
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

#[tokio::test(flavor = "multi_thread")]
async fn oauth2_payload_carries_dcr_client_id() {
    let db = TestDb::fresh().await;
    let server_id = seed_server(&db).await;
    let clock = SystemClock::shared();
    let enc = Arc::new(OrgEncryptor::for_test([7u8; 32]));
    let store: Arc<dyn McpCredentialStore> = Arc::new(PgMcpCredentialStore::new(
        db.pool.clone(),
        clock.clone(),
        enc,
    ));

    let payload = OAuth2Payload {
        access_token: "at-123".to_string(),
        refresh_token: Some("rt-456".to_string()),
        expires_at: clock.now_utc() + Duration::hours(1),
        scope: Some("read write".to_string()),
        issuer: "https://example.test".to_string(),
        token_endpoint: "https://example.test/token".to_string(),
        dcr_client_id: Some("dcr-client-789".to_string()),
        dcr_client_secret: Some("dcr-secret-xyz".to_string()),
        token_endpoint_auth_method: Some(TokenAuthMethod::ClientSecretPost),
    };

    store
        .upsert(McpCredentialWrite {
            server_id,
            org_id: db.default_org_id,
            payload: CredentialPayload::Oauth2(payload),
        })
        .await
        .expect("upsert");

    let record = store
        .read(server_id, db.default_org_id)
        .await
        .expect("read")
        .expect("row present");

    let CredentialPayload::Oauth2(got) = record.payload else {
        panic!("variant mismatch");
    };
    assert_eq!(got.access_token, "at-123");
    assert_eq!(got.refresh_token.as_deref(), Some("rt-456"));
    assert_eq!(got.dcr_client_id.as_deref(), Some("dcr-client-789"));
    assert_eq!(got.dcr_client_secret.as_deref(), Some("dcr-secret-xyz"));
    assert_eq!(
        got.token_endpoint_auth_method,
        Some(TokenAuthMethod::ClientSecretPost)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn oauth2_payload_without_dcr_fields_still_roundtrips() {
    // Platform-credential entries leave the DCR fields `None` — confirm
    // the absence path round-trips cleanly too.
    let db = TestDb::fresh().await;
    let server_id = seed_server(&db).await;
    let clock = SystemClock::shared();
    let enc = Arc::new(OrgEncryptor::for_test([9u8; 32]));
    let store: Arc<dyn McpCredentialStore> = Arc::new(PgMcpCredentialStore::new(
        db.pool.clone(),
        clock.clone(),
        enc,
    ));

    let payload = OAuth2Payload {
        access_token: "at-platform".to_string(),
        refresh_token: None,
        expires_at: clock.now_utc() + Duration::hours(1),
        scope: None,
        issuer: "https://accounts.google.com".to_string(),
        token_endpoint: "https://oauth2.googleapis.com/token".to_string(),
        dcr_client_id: None,
        dcr_client_secret: None,
        token_endpoint_auth_method: None,
    };

    store
        .upsert(McpCredentialWrite {
            server_id,
            org_id: db.default_org_id,
            payload: CredentialPayload::Oauth2(payload),
        })
        .await
        .expect("upsert");

    let CredentialPayload::Oauth2(got) = store
        .read(server_id, db.default_org_id)
        .await
        .expect("read")
        .expect("row present")
        .payload
    else {
        panic!("variant mismatch");
    };
    assert!(got.dcr_client_id.is_none());
    assert!(got.dcr_client_secret.is_none());
    assert!(got.token_endpoint_auth_method.is_none());
}
