//! Postgres-backed impls of the OAuth stores.

use std::fmt;

use async_trait::async_trait;
use sqlx::PgPool;

use crate::agents::AgentId;
use crate::auth::{OrgId, UserId};
use crate::clock::SharedClock;
use crate::crypto::{EncryptedBlob, SharedOrgEncryptor};
use crate::mcp::McpServerId;
use crate::session::SessionId;
use crate::types::SecretString;

use super::errors::OAuthError;
use super::store::{
    ClientProvenance, DcrClientRecord, McpOAuthClientStore, McpOAuthPendingStore, NewOAuthClient,
    OAuthClientId, PendingAuthorization, PendingAuthorizationWrite, ResumeCtx, SlackPingCtx,
    TokenAuthMethod,
};

pub struct PgMcpOAuthClientStore {
    pool: PgPool,
    clock: SharedClock,
    enc: SharedOrgEncryptor,
}

impl PgMcpOAuthClientStore {
    #[must_use]
    pub fn new(pool: PgPool, clock: SharedClock, enc: SharedOrgEncryptor) -> Self {
        Self { pool, clock, enc }
    }
}

impl fmt::Debug for PgMcpOAuthClientStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PgMcpOAuthClientStore")
            .finish_non_exhaustive()
    }
}

/// Insert-or-return: DCR is idempotent per `(org, issuer)`. The no-op
/// `SET issuer = issuer` forces RETURNING to fire for the existing row.
/// Conflict target is the partial unique index, qualified by the
/// org-scoped predicate so it cannot collide with shared rows.
const SQL_UPSERT_DCR: &str = "INSERT INTO mcp_oauth_clients \
     (org_id, issuer, client_id, authorization_endpoint, token_endpoint, \
      registration_client_uri, registration_access_token_ciphertext, \
      registration_access_token_nonce, client_secret_ciphertext, \
      client_secret_nonce, key_version, token_endpoint_auth_method, scope, \
      created_at) \
     VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14) \
     ON CONFLICT (org_id, issuer) WHERE org_id IS NOT NULL \
        DO UPDATE SET issuer = mcp_oauth_clients.issuer \
     RETURNING org_id, issuer, client_id, authorization_endpoint, \
               token_endpoint, client_secret_ciphertext, client_secret_nonce, \
               key_version, token_endpoint_auth_method, scope";

/// Operator-supplied: full overwrite. The caller passes `None` for
/// `registration_*` binds (operator provenance carries no such fields by
/// construction), so VALUES is identical to the DCR shape; only the
/// `ON CONFLICT` clause differs.
const SQL_UPSERT_OPERATOR: &str = "INSERT INTO mcp_oauth_clients \
     (org_id, issuer, client_id, authorization_endpoint, token_endpoint, \
      registration_client_uri, registration_access_token_ciphertext, \
      registration_access_token_nonce, client_secret_ciphertext, \
      client_secret_nonce, key_version, token_endpoint_auth_method, scope, \
      created_at) \
     VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14) \
     ON CONFLICT (org_id, issuer) WHERE org_id IS NOT NULL DO UPDATE SET \
        client_id = EXCLUDED.client_id, \
        authorization_endpoint = EXCLUDED.authorization_endpoint, \
        token_endpoint = EXCLUDED.token_endpoint, \
        registration_client_uri = NULL, \
        registration_access_token_ciphertext = NULL, \
        registration_access_token_nonce = NULL, \
        client_secret_ciphertext = EXCLUDED.client_secret_ciphertext, \
        client_secret_nonce = EXCLUDED.client_secret_nonce, \
        key_version = EXCLUDED.key_version, \
        token_endpoint_auth_method = EXCLUDED.token_endpoint_auth_method, \
        scope = EXCLUDED.scope \
     RETURNING org_id, issuer, client_id, authorization_endpoint, \
               token_endpoint, client_secret_ciphertext, client_secret_nonce, \
               key_version, token_endpoint_auth_method, scope";

/// Shared (platform-owned): full overwrite, keyed against the
/// `org_id IS NULL` row per the `mcp_oauth_clients_shared_issuer_key`
/// partial unique index. The seeder is the only writer; rotating the
/// platform-side `client_secret` (e.g. credential rotation) overwrites
/// in place rather than churning rows.
const SQL_UPSERT_SHARED: &str = "INSERT INTO mcp_oauth_clients \
     (org_id, issuer, client_id, authorization_endpoint, token_endpoint, \
      registration_client_uri, registration_access_token_ciphertext, \
      registration_access_token_nonce, client_secret_ciphertext, \
      client_secret_nonce, key_version, token_endpoint_auth_method, scope, \
      created_at) \
     VALUES (NULL, $1, $2, $3, $4, NULL, NULL, NULL, $5, $6, $7, $8, $9, $10) \
     ON CONFLICT (issuer) WHERE org_id IS NULL DO UPDATE SET \
        client_id = EXCLUDED.client_id, \
        authorization_endpoint = EXCLUDED.authorization_endpoint, \
        token_endpoint = EXCLUDED.token_endpoint, \
        client_secret_ciphertext = EXCLUDED.client_secret_ciphertext, \
        client_secret_nonce = EXCLUDED.client_secret_nonce, \
        key_version = EXCLUDED.key_version, \
        token_endpoint_auth_method = EXCLUDED.token_endpoint_auth_method, \
        scope = EXCLUDED.scope \
     RETURNING org_id, issuer, client_id, authorization_endpoint, \
               token_endpoint, client_secret_ciphertext, client_secret_nonce, \
               key_version, token_endpoint_auth_method, scope";

/// Sentinel `OrgId` for the per-org KEK derivation of shared
/// (platform-owned) rows. The nil UUID cannot collide with any real
/// org (orgs are created via `gen_random_uuid()` / `Uuid::new_v4`),
/// so the derived HKDF KEK is unique to shared-row encryption.
#[inline]
fn platform_sentinel_org() -> OrgId {
    OrgId::from(uuid::Uuid::nil())
}

use super::canonical_issuer;

#[async_trait]
impl McpOAuthClientStore for PgMcpOAuthClientStore {
    async fn upsert(&self, new: NewOAuthClient) -> Result<DcrClientRecord, OAuthError> {
        match &new.provenance {
            ClientProvenance::Shared => self.upsert_shared(new).await,
            ClientProvenance::Dcr { .. } | ClientProvenance::Operator { .. } => {
                self.upsert_org_scoped(new).await
            }
        }
    }

    async fn read(
        &self,
        org_id: OrgId,
        issuer: &str,
    ) -> Result<Option<DcrClientRecord>, OAuthError> {
        let row = crate::auth::run_privileged::<Option<OAuthClientRow>, OAuthError>(
            &self.pool,
            async |tx| {
                Ok(sqlx::query_as::<_, OAuthClientRow>(
                    "SELECT org_id, issuer, client_id, authorization_endpoint, token_endpoint, \
                            client_secret_ciphertext, client_secret_nonce, key_version, \
                            token_endpoint_auth_method, scope \
                     FROM mcp_oauth_clients WHERE org_id = $1 AND issuer = $2",
                )
                .bind(org_id)
                .bind(canonical_issuer(issuer))
                .fetch_optional(&mut **tx)
                .await?)
            },
        )
        .await?;
        row.map(|r| r.into_record(&self.enc)).transpose()
    }

    async fn read_shared(&self, issuer: &str) -> Result<Option<DcrClientRecord>, OAuthError> {
        let row = crate::auth::run_privileged::<Option<OAuthClientRow>, OAuthError>(
            &self.pool,
            async |tx| {
                Ok(sqlx::query_as::<_, OAuthClientRow>(
                    "SELECT org_id, issuer, client_id, authorization_endpoint, token_endpoint, \
                            client_secret_ciphertext, client_secret_nonce, key_version, \
                            token_endpoint_auth_method, scope \
                     FROM mcp_oauth_clients WHERE org_id IS NULL AND issuer = $1",
                )
                .bind(canonical_issuer(issuer))
                .fetch_optional(&mut **tx)
                .await?)
            },
        )
        .await?;
        row.map(|r| r.into_record(&self.enc)).transpose()
    }
}

impl PgMcpOAuthClientStore {
    /// DCR / Operator path. Keyed by `(org_id, issuer)`. `org_id` and
    /// the per-shape `(rcu, rat)` columns are read off the variant —
    /// `Shared` is routed away by `upsert` so the type system
    /// guarantees only the two org-scoped shapes reach this fn.
    async fn upsert_org_scoped(&self, new: NewOAuthClient) -> Result<DcrClientRecord, OAuthError> {
        let (org_id, rcu, rat, sql) = match &new.provenance {
            ClientProvenance::Dcr {
                org_id,
                registration_client_uri,
                registration_access_token,
            } => (
                *org_id,
                registration_client_uri.as_deref(),
                registration_access_token.as_ref(),
                SQL_UPSERT_DCR,
            ),
            ClientProvenance::Operator { org_id } => (*org_id, None, None, SQL_UPSERT_OPERATOR),
            // The outer match in `upsert` already routed Shared away.
            ClientProvenance::Shared => {
                unreachable!("invariant: upsert_org_scoped called with Shared provenance")
            }
        };
        let (secret_cipher, secret_nonce) =
            seal_optional(&self.enc, org_id, new.client_secret.as_ref())?;
        let (rat_cipher, rat_nonce) = seal_optional(&self.enc, org_id, rat)?;
        let now = self.clock.now_utc();
        let key_version = crate::crypto::CURRENT_KEY_VERSION;
        let row =
            crate::auth::run_privileged::<OAuthClientRow, OAuthError>(&self.pool, async |tx| {
                Ok(sqlx::query_as::<_, OAuthClientRow>(sql)
                    .bind(org_id)
                    .bind(canonical_issuer(&new.issuer))
                    .bind(new.client_id.as_str())
                    .bind(&new.authorization_endpoint)
                    .bind(&new.token_endpoint)
                    .bind(rcu)
                    .bind(rat_cipher.as_deref())
                    .bind(rat_nonce.as_deref())
                    .bind(secret_cipher.as_deref())
                    .bind(secret_nonce.as_deref())
                    .bind(key_version)
                    .bind(new.token_endpoint_auth_method)
                    .bind(new.scope.as_deref())
                    .bind(now)
                    .fetch_one(&mut **tx)
                    .await?)
            })
            .await?;
        row.into_record(&self.enc)
    }

    /// Shared path. Keyed by `(issuer)` against the `org_id IS NULL`
    /// partial unique index. The client_secret is sealed under the
    /// platform sentinel KEK so subsequent reads (which carry no org)
    /// can decrypt without a per-tenant context. Provenance carries no
    /// org_id by type so there is nothing to assert on the input.
    async fn upsert_shared(&self, new: NewOAuthClient) -> Result<DcrClientRecord, OAuthError> {
        let sentinel = platform_sentinel_org();
        let (secret_cipher, secret_nonce) =
            seal_optional(&self.enc, sentinel, new.client_secret.as_ref())?;
        let now = self.clock.now_utc();
        let key_version = crate::crypto::CURRENT_KEY_VERSION;
        let row =
            crate::auth::run_privileged::<OAuthClientRow, OAuthError>(&self.pool, async |tx| {
                Ok(sqlx::query_as::<_, OAuthClientRow>(SQL_UPSERT_SHARED)
                    .bind(canonical_issuer(&new.issuer))
                    .bind(new.client_id.as_str())
                    .bind(&new.authorization_endpoint)
                    .bind(&new.token_endpoint)
                    .bind(secret_cipher.as_deref())
                    .bind(secret_nonce.as_deref())
                    .bind(key_version)
                    .bind(new.token_endpoint_auth_method)
                    .bind(new.scope.as_deref())
                    .bind(now)
                    .fetch_one(&mut **tx)
                    .await?)
            })
            .await?;
        row.into_record(&self.enc)
    }
}

/// Pair of (ciphertext, nonce) bytes for an optional `SecretString`
/// column. Tuple is concrete so the call site stays readable; the
/// inner `Option`s pair the column nullability invariant (both NULL or
/// both set), enforced by the schema's CHECK clauses.
type SealedColumn = (Option<Vec<u8>>, Option<Vec<u8>>);

fn seal_optional(
    enc: &SharedOrgEncryptor,
    org: OrgId,
    plaintext: Option<&SecretString>,
) -> Result<SealedColumn, OAuthError> {
    let Some(s) = plaintext else {
        return Ok((None, None));
    };
    let blob = enc.seal(org, s.expose().as_bytes())?;
    Ok((Some(blob.ciphertext), Some(blob.nonce.to_vec())))
}

#[derive(sqlx::FromRow)]
struct OAuthClientRow {
    org_id: Option<OrgId>,
    issuer: String,
    client_id: String,
    authorization_endpoint: String,
    token_endpoint: String,
    client_secret_ciphertext: Option<Vec<u8>>,
    client_secret_nonce: Option<Vec<u8>>,
    key_version: i16,
    token_endpoint_auth_method: TokenAuthMethod,
    scope: Option<String>,
}

impl OAuthClientRow {
    fn into_record(self, enc: &SharedOrgEncryptor) -> Result<DcrClientRecord, OAuthError> {
        let client_secret = self.decode_client_secret(enc)?;
        // We only ever write valid OAuthClientId-shaped values; a row
        // that fails reconstruction here means DB-side corruption or a
        // direct INSERT, both of which are operator errors worth
        // surfacing as Misconfigured rather than a 500-on-read.
        let client_id = OAuthClientId::try_from(self.client_id).map_err(|e| {
            OAuthError::Misconfigured(format!("stored oauth client_id rejected: {e}"))
        })?;
        Ok(DcrClientRecord {
            org_id: self.org_id,
            issuer: self.issuer,
            client_id,
            client_secret,
            authorization_endpoint: self.authorization_endpoint,
            token_endpoint: self.token_endpoint,
            token_endpoint_auth_method: self.token_endpoint_auth_method,
            scope: self.scope,
        })
    }

    fn decode_client_secret(
        &self,
        enc: &SharedOrgEncryptor,
    ) -> Result<Option<SecretString>, OAuthError> {
        let (Some(c), Some(n)) = (
            self.client_secret_ciphertext.as_ref(),
            self.client_secret_nonce.as_ref(),
        ) else {
            return Ok(None);
        };
        let nonce: [u8; crate::crypto::NONCE_BYTES] = n.as_slice().try_into().map_err(|_| {
            OAuthError::Misconfigured("oauth client_secret nonce wrong length".into())
        })?;
        let blob = EncryptedBlob {
            key_version: self.key_version,
            nonce,
            ciphertext: c.clone(),
        };
        // Shared rows are sealed under the platform sentinel KEK
        // because there is no per-tenant context at write time.
        let kek_org = self.org_id.unwrap_or_else(platform_sentinel_org);
        let plaintext = enc.open(kek_org, &blob)?;
        let s = std::str::from_utf8(plaintext.as_slice())
            .map_err(|_| OAuthError::Misconfigured("oauth client_secret not utf-8".into()))?;
        Ok(Some(SecretString::try_from(s.to_owned()).map_err(|e| {
            OAuthError::Misconfigured(format!("oauth client_secret invalid: {e}"))
        })?))
    }
}

pub struct PgMcpOAuthPendingStore {
    pool: PgPool,
    clock: SharedClock,
}

impl PgMcpOAuthPendingStore {
    #[must_use]
    pub fn new(pool: PgPool, clock: SharedClock) -> Self {
        Self { pool, clock }
    }
}

impl fmt::Debug for PgMcpOAuthPendingStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PgMcpOAuthPendingStore")
            .finish_non_exhaustive()
    }
}

#[async_trait]
impl McpOAuthPendingStore for PgMcpOAuthPendingStore {
    async fn insert(&self, row: PendingAuthorizationWrite) -> Result<(), OAuthError> {
        let now = self.clock.now_utc();
        crate::auth::run_privileged::<(), OAuthError>(&self.pool, async |tx| {
            sqlx::query(
                "INSERT INTO mcp_oauth_pending \
                 (state, server_id, user_id, org_id, issuer, pkce_verifier, redirect_to, \
                  created_at, expires_at, session_id, agent_id, \
                  slack_team_id, slack_channel_id, slack_thread_ts) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14)",
            )
            .bind(&row.state)
            .bind(row.server_id)
            .bind(row.user_id)
            .bind(row.org_id)
            .bind(&row.issuer)
            .bind(&row.pkce_verifier)
            .bind(row.redirect_to.as_deref())
            .bind(now)
            .bind(row.expires_at)
            .bind(row.resume_ctx.map(|r| r.session_id))
            .bind(row.resume_ctx.map(|r| r.agent_id))
            .bind(row.slack_ctx.as_ref().map(|s| s.team_id.as_str()))
            .bind(row.slack_ctx.as_ref().map(|s| s.channel_id.as_str()))
            .bind(row.slack_ctx.as_ref().map(|s| s.thread_ts.as_str()))
            .execute(&mut **tx)
            .await?;
            Ok(())
        })
        .await
    }

    async fn consume(
        &self,
        state: &str,
        now: chrono::DateTime<chrono::Utc>,
    ) -> Result<Option<PendingAuthorization>, OAuthError> {
        crate::auth::run_privileged::<Option<PendingAuthorization>, OAuthError>(
            &self.pool,
            async |tx| {
                let row = sqlx::query_as::<_, PendingRow>(
                    "DELETE FROM mcp_oauth_pending \
                     WHERE state = $1 AND expires_at > $2 \
                     RETURNING state, server_id, user_id, org_id, issuer, pkce_verifier, \
                               redirect_to, session_id, agent_id, \
                               slack_team_id, slack_channel_id, slack_thread_ts",
                )
                .bind(state)
                .bind(now)
                .fetch_optional(&mut **tx)
                .await?;
                Ok(row.map(PendingRow::into_record))
            },
        )
        .await
    }
}

#[derive(sqlx::FromRow)]
struct PendingRow {
    state: String,
    server_id: McpServerId,
    user_id: UserId,
    org_id: OrgId,
    issuer: String,
    pkce_verifier: String,
    redirect_to: Option<String>,
    session_id: Option<SessionId>,
    agent_id: Option<AgentId>,
    slack_team_id: Option<String>,
    slack_channel_id: Option<String>,
    slack_thread_ts: Option<String>,
}

impl PendingRow {
    fn into_record(self) -> PendingAuthorization {
        // The all-or-none CHECK constraints guarantee each pair / triple
        // is populated together — if the DB ever serves a half-populated
        // shape it's a schema-vs-code divergence (§6 assertion).
        let resume_ctx = match (self.session_id, self.agent_id) {
            (Some(session_id), Some(agent_id)) => Some(ResumeCtx {
                session_id,
                agent_id,
            }),
            (None, None) => None,
            _ => panic!(
                "invariant: mcp_oauth_pending.resume_ctx half-populated; \
                 CHECK constraint violated"
            ),
        };
        let slack_ctx = match (
            self.slack_team_id,
            self.slack_channel_id,
            self.slack_thread_ts,
        ) {
            (Some(team_id), Some(channel_id), Some(thread_ts)) => Some(SlackPingCtx {
                team_id,
                channel_id,
                thread_ts,
            }),
            (None, None, None) => None,
            _ => panic!(
                "invariant: mcp_oauth_pending.slack_ctx partially-populated; \
                 CHECK constraint violated"
            ),
        };
        PendingAuthorization {
            state: self.state,
            server_id: self.server_id,
            user_id: self.user_id,
            org_id: self.org_id,
            issuer: self.issuer,
            pkce_verifier: self.pkce_verifier,
            redirect_to: self.redirect_to,
            resume_ctx,
            slack_ctx,
        }
    }
}
