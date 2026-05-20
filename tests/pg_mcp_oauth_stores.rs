//! Postgres roundtrip tests for the MCP OAuth tables (R3 — phase C).
//!
//! Exercises:
//!   - `mcp_oauth_clients` upsert + idempotent re-upsert (no duplicate row).
//!   - Encrypted `client_secret` decrypts back to the same plaintext.
//!   - `mcp_oauth_pending` insert + one-shot `consume` (replay fails).
//!   - Expired pending rows are surfaced as `None` from `consume`.

#![allow(clippy::expect_used)]

use std::sync::Arc;

use relay_rs::clock::SystemClock;
use relay_rs::crypto::OrgEncryptor;
use relay_rs::mcp::oauth::{
    ClientProvenance, McpOAuthClientStore, McpOAuthPendingStore, NewOAuthClient, OAuthClientId,
    PendingAuthorizationWrite, PgMcpOAuthClientStore, PgMcpOAuthPendingStore, TokenAuthMethod,
};
use relay_rs::mcp::{
    ConnectionStatus, McpCatalogId, McpHttpUrl, McpServerCreate, McpServerStore, McpTransport,
    PgMcpServerStore,
};
use relay_rs::types::SecretString;

mod common;
use common::pg::TestDb;

fn encryptor() -> Arc<OrgEncryptor> {
    Arc::new(OrgEncryptor::for_test([9u8; 32]))
}

/// Construct a populated `NewOAuthClient` for one issuer. Tests vary
/// fields by mutating the returned value — extending this fixture is the
/// path for any future vendor quirk test (one issuer, one mutation).
fn client_fixture(
    org_id: relay_rs::auth::OrgId,
    issuer: &str,
    client_id: &str,
    secret: Option<&str>,
    provenance: ClientProvenance,
) -> NewOAuthClient {
    NewOAuthClient {
        org_id,
        issuer: issuer.to_owned(),
        client_id: OAuthClientId::try_from(client_id.to_owned()).expect("valid client_id"),
        client_secret: secret.map(|s| SecretString::try_from(s.to_owned()).expect("valid secret")),
        authorization_endpoint: format!("{issuer}/auth"),
        token_endpoint: format!("{issuer}/token"),
        token_endpoint_auth_method: TokenAuthMethod::ClientSecretBasic,
        scope: None,
        provenance,
    }
}

fn dcr_provenance() -> ClientProvenance {
    ClientProvenance::Dcr {
        registration_client_uri: None,
        registration_access_token: None,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn oauth_client_upsert_then_read_returns_decrypted_secret() {
    let db = TestDb::fresh().await;
    let store = PgMcpOAuthClientStore::new(db.pool.clone(), SystemClock::shared(), encryptor());

    let mut new = client_fixture(
        db.default_org_id,
        "https://issuer.example",
        "client-xyz",
        Some("sekret-1"),
        dcr_provenance(),
    );
    new.scope = Some("read write".into());
    let row = store.upsert(new).await.expect("upsert");
    assert_eq!(row.client_id.as_str(), "client-xyz");
    assert_eq!(row.scope.as_deref(), Some("read write"));
    assert_eq!(row.client_secret.expect("secret").expose(), "sekret-1");

    let again = store
        .read(db.default_org_id, "https://issuer.example")
        .await
        .expect("read")
        .expect("present");
    assert_eq!(again.client_id.as_str(), "client-xyz");
    assert_eq!(again.client_secret.expect("secret").expose(), "sekret-1");
}

#[tokio::test(flavor = "multi_thread")]
async fn oauth_client_dcr_upsert_is_idempotent_per_issuer() {
    let db = TestDb::fresh().await;
    let store = PgMcpOAuthClientStore::new(db.pool.clone(), SystemClock::shared(), encryptor());

    let first = store
        .upsert(client_fixture(
            db.default_org_id,
            "https://issuer.example",
            "first-id",
            Some("first-secret"),
            dcr_provenance(),
        ))
        .await
        .expect("first upsert");
    let second = store
        .upsert(client_fixture(
            db.default_org_id,
            "https://issuer.example",
            "second-id",
            Some("second-secret"),
            dcr_provenance(),
        ))
        .await
        .expect("second upsert");
    // DCR provenance is insert-or-return: the second call returns the
    // first row verbatim, never the would-be replacement.
    assert_eq!(first.client_id, second.client_id);
    assert_eq!(first.client_id.as_str(), "first-id");
}

async fn seed_server(db: &TestDb) -> relay_rs::mcp::McpServerId {
    let server_store = PgMcpServerStore::new(db.pool.clone(), SystemClock::shared());
    let row = server_store
        .create(McpServerCreate {
            org_id: db.default_org_id,
            created_by_user_id: db.default_user_id,
            // Migration 30 seeds `notion` globally, so it satisfies the
            // FK trigger without per-test seed work.
            catalog_id: McpCatalogId::try_from("notion").expect("catalog id"),
            config: McpTransport::Http {
                url: McpHttpUrl::try_from("http://localhost:9000").expect("url"),
            },
            description: None,
            enabled: true,
            connection_status: ConnectionStatus::Ok,
        })
        .await
        .expect("seed server");
    row.id
}

#[tokio::test(flavor = "multi_thread")]
async fn oauth_pending_insert_then_consume_returns_row() {
    let db = TestDb::fresh().await;
    let server_id = seed_server(&db).await;
    let clock = SystemClock::shared();
    let store = PgMcpOAuthPendingStore::new(db.pool.clone(), clock.clone());
    let now = clock.now_utc();
    store
        .insert(PendingAuthorizationWrite {
            state: "a".repeat(40),
            server_id,
            user_id: db.default_user_id,
            org_id: db.default_org_id,
            issuer: "https://issuer.example".into(),
            pkce_verifier: "v".repeat(43),
            redirect_to: Some("/settings".into()),
            expires_at: now + chrono::Duration::seconds(120),
        })
        .await
        .expect("insert");
    let row = store
        .consume(&"a".repeat(40), now)
        .await
        .expect("consume")
        .expect("row present");
    assert_eq!(row.server_id, server_id);
    assert_eq!(row.issuer, "https://issuer.example");
    // Second consume returns None — the row was deleted on the first read.
    let dup = store
        .consume(&"a".repeat(40), now)
        .await
        .expect("second consume");
    assert!(dup.is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn oauth_client_operator_provenance_overwrites_existing_row() {
    let db = TestDb::fresh().await;
    let store = PgMcpOAuthClientStore::new(db.pool.clone(), SystemClock::shared(), encryptor());

    // Seed via DCR with populated registration_* fields.
    let mut seed = client_fixture(
        db.default_org_id,
        "https://issuer.example",
        "dcr-id",
        Some("dcr-secret"),
        ClientProvenance::Dcr {
            registration_client_uri: Some("https://issuer.example/clients/123".into()),
            registration_access_token: Some(
                SecretString::try_from("reg-tok".to_owned()).expect("valid"),
            ),
        },
    );
    seed.token_endpoint_auth_method = TokenAuthMethod::ClientSecretBasic;
    store.upsert(seed).await.expect("dcr seed");

    // Operator overwrite: replaces every operator-visible column. The
    // store forces registration_* back to NULL by construction
    // (no field on `ClientProvenance::Operator`).
    let mut new = client_fixture(
        db.default_org_id,
        "https://issuer.example",
        "operator-id",
        Some("operator-secret"),
        ClientProvenance::Operator,
    );
    new.token_endpoint_auth_method = TokenAuthMethod::ClientSecretPost;
    new.scope = Some("channels:read".into());
    new.authorization_endpoint = "https://issuer.example/v2/auth".into();
    new.token_endpoint = "https://issuer.example/v2/token".into();
    let replaced = store.upsert(new).await.expect("operator upsert");

    assert_eq!(replaced.client_id.as_str(), "operator-id");
    assert_eq!(
        replaced.client_secret.expect("secret").expose(),
        "operator-secret"
    );
    assert_eq!(
        replaced.token_endpoint_auth_method,
        TokenAuthMethod::ClientSecretPost
    );
    assert_eq!(replaced.scope.as_deref(), Some("channels:read"));
    assert_eq!(replaced.token_endpoint, "https://issuer.example/v2/token");

    // Operator overwrite zeroes the DCR-only columns. Hit the DB
    // directly because `DcrClientRecord` deliberately doesn't surface
    // them — the contract is "stored as NULL", not "absent from the
    // domain type".
    let reg: (Option<String>, Option<Vec<u8>>, Option<Vec<u8>>) = sqlx::query_as(
        "SELECT registration_client_uri, \
                registration_access_token_ciphertext, \
                registration_access_token_nonce \
         FROM mcp_oauth_clients WHERE org_id = $1 AND issuer = $2",
    )
    .bind(db.default_org_id)
    .bind("https://issuer.example")
    .fetch_one(&db.pool)
    .await
    .expect("registration columns");
    assert!(reg.0.is_none(), "registration_client_uri must be NULL");
    assert!(
        reg.1.is_none(),
        "registration_access_token_ciphertext must be NULL"
    );
    assert!(
        reg.2.is_none(),
        "registration_access_token_nonce must be NULL"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn oauth_pending_expired_rows_yield_none() {
    let db = TestDb::fresh().await;
    let server_id = seed_server(&db).await;
    let clock = SystemClock::shared();
    let store = PgMcpOAuthPendingStore::new(db.pool.clone(), clock.clone());
    let now = clock.now_utc();
    store
        .insert(PendingAuthorizationWrite {
            state: "b".repeat(40),
            server_id,
            user_id: db.default_user_id,
            org_id: db.default_org_id,
            issuer: "https://issuer.example".into(),
            pkce_verifier: "v".repeat(43),
            redirect_to: None,
            expires_at: now - chrono::Duration::seconds(1),
        })
        .await
        .expect("insert");
    let row = store.consume(&"b".repeat(40), now).await.expect("consume");
    assert!(row.is_none(), "expired rows must not be returned");
}
