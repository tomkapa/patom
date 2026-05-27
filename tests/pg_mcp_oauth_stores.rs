//! Postgres roundtrip tests for the MCP OAuth tables (R3 — phase C).
//!
//! Exercises:
//!   - `mcp_oauth_clients` upsert + idempotent re-upsert (no duplicate row).
//!   - Encrypted `client_secret` decrypts back to the same plaintext.
//!   - `mcp_oauth_pending` insert + one-shot `consume` (replay fails).
//!   - Expired pending rows are surfaced as `None` from `consume`.

#![allow(clippy::expect_used)]

use std::sync::Arc;

use patom_rs::clock::SystemClock;
use patom_rs::crypto::OrgEncryptor;
use patom_rs::mcp::oauth::{
    ClientProvenance, McpOAuthClientStore, McpOAuthPendingStore, NewOAuthClient, OAuthClientId,
    PendingAuthorizationWrite, PgMcpOAuthClientStore, PgMcpOAuthPendingStore, TokenAuthMethod,
};
use patom_rs::mcp::{
    ConnectionStatus, McpCatalogId, McpHttpUrl, McpServerCreate, McpServerStore, McpTransport,
    PgMcpServerStore,
};
use patom_rs::types::SecretString;

mod common;
use common::pg::TestDb;

fn encryptor() -> Arc<OrgEncryptor> {
    Arc::new(OrgEncryptor::for_test([9u8; 32]))
}

/// Construct a populated `NewOAuthClient` for one issuer. Tests vary
/// fields by mutating the returned value — extending this fixture is the
/// path for any future vendor quirk test (one issuer, one mutation).
fn client_fixture(
    issuer: &str,
    client_id: &str,
    secret: Option<&str>,
    provenance: ClientProvenance,
) -> NewOAuthClient {
    NewOAuthClient {
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

fn dcr_provenance(org_id: patom_rs::auth::OrgId) -> ClientProvenance {
    ClientProvenance::Dcr {
        org_id,
        registration_client_uri: None,
        registration_access_token: None,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn oauth_client_upsert_then_read_returns_decrypted_secret() {
    let db = TestDb::fresh().await;
    let store = PgMcpOAuthClientStore::new(db.pool.clone(), SystemClock::shared(), encryptor());

    let mut new = client_fixture(
        "https://issuer.example",
        "client-xyz",
        Some("sekret-1"),
        dcr_provenance(db.default_org_id),
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
            "https://issuer.example",
            "first-id",
            Some("first-secret"),
            dcr_provenance(db.default_org_id),
        ))
        .await
        .expect("first upsert");
    let second = store
        .upsert(client_fixture(
            "https://issuer.example",
            "second-id",
            Some("second-secret"),
            dcr_provenance(db.default_org_id),
        ))
        .await
        .expect("second upsert");
    // DCR provenance is insert-or-return: the second call returns the
    // first row verbatim, never the would-be replacement.
    assert_eq!(first.client_id, second.client_id);
    assert_eq!(first.client_id.as_str(), "first-id");
}

async fn seed_server(db: &TestDb) -> patom_rs::mcp::McpServerId {
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
            resume_ctx: None,
            slack_ctx: None,
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
        "https://issuer.example",
        "dcr-id",
        Some("dcr-secret"),
        ClientProvenance::Dcr {
            org_id: db.default_org_id,
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
        "https://issuer.example",
        "operator-id",
        Some("operator-secret"),
        ClientProvenance::Operator {
            org_id: db.default_org_id,
        },
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
            resume_ctx: None,
            slack_ctx: None,
        })
        .await
        .expect("insert");
    let row = store.consume(&"b".repeat(40), now).await.expect("consume");
    assert!(row.is_none(), "expired rows must not be returned");
}

fn shared_fixture(issuer: &str, client_id: &str, secret: &str) -> NewOAuthClient {
    NewOAuthClient {
        issuer: issuer.to_owned(),
        client_id: OAuthClientId::try_from(client_id.to_owned()).expect("valid client_id"),
        client_secret: Some(SecretString::try_from(secret.to_owned()).expect("valid secret")),
        authorization_endpoint: format!("{issuer}/auth"),
        token_endpoint: format!("{issuer}/token"),
        token_endpoint_auth_method: TokenAuthMethod::ClientSecretPost,
        scope: None,
        provenance: ClientProvenance::Shared,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn shared_client_upsert_then_read_shared_decrypts_secret() {
    let db = TestDb::fresh().await;
    let store = PgMcpOAuthClientStore::new(db.pool.clone(), SystemClock::shared(), encryptor());

    let written = store
        .upsert(shared_fixture(
            "https://accounts.google.com/",
            "google-client-id",
            "google-secret",
        ))
        .await
        .expect("shared upsert");
    assert!(
        written.org_id.is_none(),
        "shared rows must persist with org_id IS NULL"
    );

    let fetched = store
        .read_shared("https://accounts.google.com/")
        .await
        .expect("read_shared")
        .expect("row present");
    assert!(fetched.org_id.is_none());
    assert_eq!(fetched.client_id.as_str(), "google-client-id");
    assert_eq!(
        fetched.client_secret.expect("secret").expose(),
        "google-secret"
    );
    assert_eq!(
        fetched.token_endpoint_auth_method,
        TokenAuthMethod::ClientSecretPost
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn shared_client_upsert_is_idempotent_and_overwrites_secret() {
    let db = TestDb::fresh().await;
    let store = PgMcpOAuthClientStore::new(db.pool.clone(), SystemClock::shared(), encryptor());

    store
        .upsert(shared_fixture(
            "https://accounts.google.com/",
            "id-v1",
            "v1",
        ))
        .await
        .expect("first");
    let second = store
        .upsert(shared_fixture(
            "https://accounts.google.com/",
            "id-v2",
            "v2",
        ))
        .await
        .expect("second");
    // Shared upsert overwrites — credential rotation must actually
    // replace the persisted ciphertext.
    assert_eq!(second.client_id.as_str(), "id-v2");
    assert_eq!(second.client_secret.expect("secret").expose(), "v2");

    let count: (i64,) =
        sqlx::query_as("SELECT count(*) FROM mcp_oauth_clients WHERE org_id IS NULL")
            .fetch_one(&db.pool)
            .await
            .expect("count shared rows");
    assert_eq!(count.0, 1, "shared upsert must not insert duplicate rows");
}

#[tokio::test(flavor = "multi_thread")]
async fn shared_and_org_scoped_rows_coexist_under_same_issuer() {
    let db = TestDb::fresh().await;
    let store = PgMcpOAuthClientStore::new(db.pool.clone(), SystemClock::shared(), encryptor());

    let issuer = "https://accounts.google.com/";
    // Shared row (platform-owned).
    store
        .upsert(shared_fixture(issuer, "shared-id", "shared-secret"))
        .await
        .expect("shared upsert");
    // Org-scoped operator override for the same issuer.
    let mut org_scoped = client_fixture(
        issuer,
        "org-operator-id",
        Some("org-operator-secret"),
        ClientProvenance::Operator {
            org_id: db.default_org_id,
        },
    );
    org_scoped.token_endpoint_auth_method = TokenAuthMethod::ClientSecretPost;
    store
        .upsert(org_scoped)
        .await
        .expect("org-scoped operator upsert");

    // Both lookups must resolve independently — the org-scoped row does
    // NOT shadow the shared one in the table, only in the route-level
    // lookup precedence.
    let shared = store
        .read_shared(issuer)
        .await
        .expect("read_shared")
        .expect("shared row still present");
    assert_eq!(shared.client_id.as_str(), "shared-id");

    let scoped = store
        .read(db.default_org_id, issuer)
        .await
        .expect("read org-scoped")
        .expect("org-scoped row still present");
    assert_eq!(scoped.client_id.as_str(), "org-operator-id");
}

#[tokio::test(flavor = "multi_thread")]
async fn shared_row_select_visible_to_org_member_under_rls() {
    let db = TestDb::fresh().await;
    let store = PgMcpOAuthClientStore::new(db.pool.clone(), SystemClock::shared(), encryptor());
    // Canonical (no trailing slash) form — the store canonicalizes on
    // write so the raw SQL queries below need to match what's stored.
    let issuer = "https://accounts.google.com";
    store
        .upsert(shared_fixture(issuer, "id", "secret"))
        .await
        .expect("seed shared");

    // Run a SELECT under the org member's role (patom_app + app.user_id GUC).
    // The mcp_oauth_clients_select policy must admit `org_id IS NULL`
    // for any authenticated principal.
    let visible: i64 =
        patom_rs::auth::run_as_user::<i64, sqlx::Error>(&db.pool, db.default_user_id, async |tx| {
            let row: (i64,) = sqlx::query_as(
                "SELECT count(*) FROM mcp_oauth_clients \
                 WHERE org_id IS NULL AND issuer = $1",
            )
            .bind(issuer)
            .fetch_one(&mut **tx)
            .await?;
            Ok(row.0)
        })
        .await
        .expect("run as user");
    assert_eq!(
        visible, 1,
        "shared row must be visible to authenticated user"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn shared_row_writes_blocked_by_rls_for_org_member() {
    let db = TestDb::fresh().await;
    let store = PgMcpOAuthClientStore::new(db.pool.clone(), SystemClock::shared(), encryptor());
    let issuer = "https://accounts.google.com";
    store
        .upsert(shared_fixture(issuer, "id", "secret"))
        .await
        .expect("seed shared");

    // UPDATE through a tenant principal must affect zero rows: the
    // mcp_oauth_clients_update policy's WITH CHECK requires
    // `org_id IS NOT NULL` so the shared row cannot be mutated by an
    // org member, only by the migration / seeder path (run_privileged).
    let updated: u64 =
        patom_rs::auth::run_as_user::<u64, sqlx::Error>(&db.pool, db.default_user_id, async |tx| {
            let res = sqlx::query(
                "UPDATE mcp_oauth_clients SET client_id = 'tamper' \
                 WHERE org_id IS NULL AND issuer = $1",
            )
            .bind(issuer)
            .execute(&mut **tx)
            .await?;
            Ok(res.rows_affected())
        })
        .await
        .expect("run as user");
    assert_eq!(updated, 0, "org member must not UPDATE shared rows");

    // DELETE likewise must affect zero rows.
    let deleted: u64 =
        patom_rs::auth::run_as_user::<u64, sqlx::Error>(&db.pool, db.default_user_id, async |tx| {
            let res =
                sqlx::query("DELETE FROM mcp_oauth_clients WHERE org_id IS NULL AND issuer = $1")
                    .bind(issuer)
                    .execute(&mut **tx)
                    .await?;
            Ok(res.rows_affected())
        })
        .await
        .expect("run as user");
    assert_eq!(deleted, 0, "org member must not DELETE shared rows");

    // The row is still there.
    let still: i64 = patom_rs::auth::run_privileged::<i64, sqlx::Error>(&db.pool, async |tx| {
        let row: (i64,) =
            sqlx::query_as("SELECT count(*) FROM mcp_oauth_clients WHERE org_id IS NULL")
                .fetch_one(&mut **tx)
                .await?;
        Ok(row.0)
    })
    .await
    .expect("privileged count");
    assert_eq!(still, 1);
}

/// Regression: issuer comparison is trailing-slash-insensitive on both
/// sides. Google's protected-resource doc says
/// `https://accounts.google.com/` but the AS metadata self-declares as
/// `https://accounts.google.com` (no slash). Pre-fix, a single
/// character of drift made the shared-client lookup miss and fall
/// through to DCR with `DcrUnsupported`. Now the store canonicalizes
/// both writes and reads so the two forms are equivalent.
#[tokio::test(flavor = "multi_thread")]
async fn issuer_lookup_is_trailing_slash_insensitive() {
    let db = TestDb::fresh().await;
    let store = PgMcpOAuthClientStore::new(db.pool.clone(), SystemClock::shared(), encryptor());

    // Seed with NO trailing slash (matches the AS-declared form).
    store
        .upsert(shared_fixture(
            "https://accounts.google.com",
            "google-id",
            "google-secret",
        ))
        .await
        .expect("seed shared");

    // Lookup with the WITH-slash variant must resolve to the same row.
    let with_slash = store
        .read_shared("https://accounts.google.com/")
        .await
        .expect("read_shared with slash")
        .expect("row found via slash variant");
    assert_eq!(with_slash.client_id.as_str(), "google-id");

    // And the round-trip without the slash also works.
    let without_slash = store
        .read_shared("https://accounts.google.com")
        .await
        .expect("read_shared no slash")
        .expect("row found via no-slash variant");
    assert_eq!(without_slash.client_id.as_str(), "google-id");

    // Re-seeding under the WITH-slash form must hit the SAME row, not
    // insert a duplicate — otherwise the unique-index would silently
    // accept two near-identical issuers.
    store
        .upsert(shared_fixture(
            "https://accounts.google.com/",
            "google-id-rotated",
            "rotated-secret",
        ))
        .await
        .expect("re-seed shared with slash");
    let count: (i64,) =
        sqlx::query_as("SELECT count(*) FROM mcp_oauth_clients WHERE org_id IS NULL")
            .fetch_one(&db.pool)
            .await
            .expect("count shared rows");
    assert_eq!(
        count.0, 1,
        "trailing-slash variant must NOT duplicate the row",
    );
    let after_rotate = store
        .read_shared("https://accounts.google.com")
        .await
        .expect("read after rotate")
        .expect("row present");
    assert_eq!(after_rotate.client_id.as_str(), "google-id-rotated");
}
