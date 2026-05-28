//! Postgres-backed impl of [`McpOAuthPendingStore`].
//!
//! The DCR client material (client_id, client_secret, auth method,
//! endpoints) is carried on the pending row so the freshly-registered
//! client travels from `POST /oauth/start` to `GET /oauth/callback`
//! without a separate `mcp_oauth_clients` table. The client_secret is
//! sealed under the org's KEK before INSERT and opened on consume.

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
    McpOAuthPendingStore, OAuthClientId, PendingAuthorization, PendingAuthorizationWrite,
    PendingDcrClient, ResumeCtx, SlackPingCtx, TokenAuthMethod,
};

pub struct PgMcpOAuthPendingStore {
    pool: PgPool,
    clock: SharedClock,
    enc: SharedOrgEncryptor,
}

impl PgMcpOAuthPendingStore {
    #[must_use]
    pub fn new(pool: PgPool, clock: SharedClock, enc: SharedOrgEncryptor) -> Self {
        Self { pool, clock, enc }
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
        let dcr_ref = row.dcr_client.as_ref();
        let dcr_secret_blob = dcr_ref
            .and_then(|d| d.client_secret.as_ref())
            .map(|s| self.enc.seal(row.org_id, s.expose().as_bytes()))
            .transpose()?;
        let (dcr_secret_cipher, dcr_secret_nonce): (Option<Vec<u8>>, Option<Vec<u8>>) =
            dcr_secret_blob
                .map(|b| (b.ciphertext, b.nonce.to_vec()))
                .unzip();
        crate::auth::run_privileged::<(), OAuthError>(&self.pool, async |tx| {
            sqlx::query(
                "INSERT INTO mcp_oauth_pending \
                 (state, server_id, user_id, org_id, pkce_verifier, redirect_to, \
                  created_at, expires_at, session_id, agent_id, \
                  slack_team_id, slack_channel_id, slack_thread_ts, \
                  dcr_client_id, dcr_client_secret_ciphertext, dcr_client_secret_nonce, \
                  dcr_token_endpoint_auth_method, dcr_authorization_endpoint, \
                  dcr_token_endpoint) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, \
                         $14, $15, $16, $17, $18, $19)",
            )
            .bind(&row.state)
            .bind(row.server_id)
            .bind(row.user_id)
            .bind(row.org_id)
            .bind(&row.pkce_verifier)
            .bind(row.redirect_to.as_deref())
            .bind(now)
            .bind(row.expires_at)
            .bind(row.resume_ctx.map(|r| r.session_id))
            .bind(row.resume_ctx.map(|r| r.agent_id))
            .bind(row.slack_ctx.as_ref().map(|s| s.team_id.as_str()))
            .bind(row.slack_ctx.as_ref().map(|s| s.channel_id.as_str()))
            .bind(row.slack_ctx.as_ref().map(|s| s.thread_ts.as_str()))
            .bind(dcr_ref.map(|d| d.client_id.as_str()))
            .bind(dcr_secret_cipher.as_deref())
            .bind(dcr_secret_nonce.as_deref())
            .bind(dcr_ref.map(|d| d.token_endpoint_auth_method))
            .bind(dcr_ref.map(|d| d.authorization_endpoint.as_str()))
            .bind(dcr_ref.map(|d| d.token_endpoint.as_str()))
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
        let row =
            crate::auth::run_privileged::<Option<PendingRow>, OAuthError>(&self.pool, async |tx| {
                Ok(sqlx::query_as::<_, PendingRow>(
                    "DELETE FROM mcp_oauth_pending \
                     WHERE state = $1 AND expires_at > $2 \
                     RETURNING state, server_id, user_id, org_id, pkce_verifier, \
                               redirect_to, session_id, agent_id, \
                               slack_team_id, slack_channel_id, slack_thread_ts, \
                               dcr_client_id, dcr_client_secret_ciphertext, \
                               dcr_client_secret_nonce, dcr_token_endpoint_auth_method, \
                               dcr_authorization_endpoint, dcr_token_endpoint",
                )
                .bind(state)
                .bind(now)
                .fetch_optional(&mut **tx)
                .await?)
            })
            .await?;
        row.map(|r| r.into_record(&self.enc)).transpose()
    }
}

#[derive(sqlx::FromRow)]
struct PendingRow {
    state: String,
    server_id: McpServerId,
    user_id: UserId,
    org_id: OrgId,
    pkce_verifier: String,
    redirect_to: Option<String>,
    session_id: Option<SessionId>,
    agent_id: Option<AgentId>,
    slack_team_id: Option<String>,
    slack_channel_id: Option<String>,
    slack_thread_ts: Option<String>,
    dcr_client_id: Option<String>,
    dcr_client_secret_ciphertext: Option<Vec<u8>>,
    dcr_client_secret_nonce: Option<Vec<u8>>,
    dcr_token_endpoint_auth_method: Option<TokenAuthMethod>,
    dcr_authorization_endpoint: Option<String>,
    dcr_token_endpoint: Option<String>,
}

impl PendingRow {
    fn into_record(self, enc: &SharedOrgEncryptor) -> Result<PendingAuthorization, OAuthError> {
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
        let dcr_client = match (
            self.dcr_client_id,
            self.dcr_token_endpoint_auth_method,
            self.dcr_authorization_endpoint,
            self.dcr_token_endpoint,
        ) {
            (Some(client_id), Some(auth_method), Some(authz), Some(token_ep)) => {
                let client_id = OAuthClientId::try_from(client_id).map_err(|e| {
                    OAuthError::Misconfigured(format!("pending dcr client_id rejected: {e}"))
                })?;
                let client_secret = decode_pending_secret(
                    enc,
                    self.org_id,
                    self.dcr_client_secret_ciphertext.as_deref(),
                    self.dcr_client_secret_nonce.as_deref(),
                )?;
                Some(PendingDcrClient {
                    client_id,
                    client_secret,
                    token_endpoint_auth_method: auth_method,
                    authorization_endpoint: authz,
                    token_endpoint: token_ep,
                })
            }
            (None, None, None, None) => None,
            // Partial population — the
            // `mcp_oauth_pending_dcr_all_or_none` CHECK constraint
            // should keep this branch unreachable, but a future schema
            // change or an out-of-band INSERT could violate it. Return
            // a typed error rather than `panic!`; per CLAUDE.md §6
            // panic = abort would SIGABRT the process on a recoverable
            // user-visible failure.
            _ => {
                return Err(OAuthError::Misconfigured(
                    "mcp_oauth_pending.dcr_* partially populated; expected \
                     all-or-none across (dcr_client_id, dcr_token_endpoint_auth_method, \
                     dcr_authorization_endpoint, dcr_token_endpoint)"
                        .to_owned(),
                ));
            }
        };
        Ok(PendingAuthorization {
            state: self.state,
            server_id: self.server_id,
            user_id: self.user_id,
            org_id: self.org_id,
            pkce_verifier: self.pkce_verifier,
            redirect_to: self.redirect_to,
            resume_ctx,
            slack_ctx,
            dcr_client,
        })
    }
}

fn decode_pending_secret(
    enc: &SharedOrgEncryptor,
    org_id: OrgId,
    cipher: Option<&[u8]>,
    nonce: Option<&[u8]>,
) -> Result<Option<SecretString>, OAuthError> {
    let (Some(c), Some(n)) = (cipher, nonce) else {
        return Ok(None);
    };
    let nonce: [u8; crate::crypto::NONCE_BYTES] = n
        .try_into()
        .map_err(|_| OAuthError::Misconfigured("pending dcr secret nonce wrong length".into()))?;
    let blob = EncryptedBlob {
        key_version: crate::crypto::CURRENT_KEY_VERSION,
        nonce,
        ciphertext: c.to_vec(),
    };
    let plaintext = enc.open(org_id, &blob)?;
    let s = std::str::from_utf8(plaintext.as_slice())
        .map_err(|_| OAuthError::Misconfigured("pending dcr secret not utf-8".into()))?;
    Ok(Some(SecretString::try_from(s.to_owned()).map_err(|e| {
        OAuthError::Misconfigured(format!("pending dcr secret invalid: {e}"))
    })?))
}
