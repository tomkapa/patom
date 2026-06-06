//! Postgres-backed [`McpCredentialStore`].
//!
//! Holds a handle to the process-wide [`OrgEncryptor`]. Every seal/open
//! happens inside this store; nothing above the store ever sees raw
//! ciphertext bytes (callers operate on the typed [`CredentialPayload`]).

use std::fmt;

use async_trait::async_trait;
use sqlx::PgPool;

use crate::auth::OrgId;
use crate::clock::SharedClock;
use crate::crypto::{EncryptedBlob, SharedOrgEncryptor};

use super::credentials::{
    McpCredentialRecord, McpCredentialStore, McpCredentialWrite, open_payload, seal_payload,
};
use super::error::McpError;
use super::types::McpServerId;

pub struct PgMcpCredentialStore {
    pool: PgPool,
    clock: SharedClock,
    enc: SharedOrgEncryptor,
}

impl PgMcpCredentialStore {
    #[must_use]
    pub fn new(pool: PgPool, clock: SharedClock, enc: SharedOrgEncryptor) -> Self {
        Self { pool, clock, enc }
    }
}

impl fmt::Debug for PgMcpCredentialStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PgMcpCredentialStore")
            .finish_non_exhaustive()
    }
}

#[async_trait]
impl McpCredentialStore for PgMcpCredentialStore {
    async fn upsert(&self, write: McpCredentialWrite) -> Result<(), McpError> {
        let McpCredentialWrite {
            server_id,
            org_id,
            payload,
        } = write;
        let (kind, blob) = seal_payload(&self.enc, org_id, &payload)?;
        let now = self.clock.now_utc();
        crate::auth::run_privileged::<(), McpError>(&self.pool, async |tx| {
            // ON CONFLICT updates the existing row in a single statement —
            // we never read the old ciphertext back, so a replacement
            // cannot expose the prior credential. The `WHERE … org_id =
            // EXCLUDED.org_id` guard is defence-in-depth on top of the
            // parent-org trigger: a future regression that lets a
            // mismatched org_id reach this code is silently turned into
            // a no-op rather than overwriting another tenant's row, and
            // the rows_affected check below promotes that to an error.
            let affected = sqlx::query(
                "INSERT INTO mcp_server_credentials \
                 (server_id, org_id, kind, ciphertext, nonce, key_version, created_at, updated_at) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $7) \
                 ON CONFLICT (server_id) DO UPDATE SET \
                     kind = EXCLUDED.kind, \
                     ciphertext = EXCLUDED.ciphertext, \
                     nonce = EXCLUDED.nonce, \
                     key_version = EXCLUDED.key_version, \
                     updated_at = EXCLUDED.updated_at \
                 WHERE mcp_server_credentials.org_id = EXCLUDED.org_id",
            )
            .bind(server_id)
            .bind(org_id)
            .bind(kind)
            .bind(&blob.ciphertext)
            .bind(&blob.nonce[..])
            .bind(blob.key_version)
            .bind(now)
            .execute(&mut **tx)
            .await?
            .rows_affected();
            if affected == 0 {
                return Err(McpError::Backend(
                    "credential upsert: org_id mismatch on existing row".into(),
                ));
            }
            Ok(())
        })
        .await
    }

    async fn delete(&self, server_id: McpServerId, org_id: OrgId) -> Result<(), McpError> {
        crate::auth::run_privileged::<(), McpError>(&self.pool, async |tx| {
            sqlx::query("DELETE FROM mcp_server_credentials WHERE server_id = $1 AND org_id = $2")
                .bind(server_id)
                .bind(org_id)
                .execute(&mut **tx)
                .await?;
            Ok(())
        })
        .await
    }

    async fn list_all(&self) -> Result<Vec<McpCredentialRecord>, McpError> {
        let rows =
            crate::auth::run_privileged::<Vec<CredentialRow>, McpError>(&self.pool, async |tx| {
                Ok(sqlx::query_as::<_, CredentialRow>(
                    "SELECT server_id, org_id, kind, ciphertext, nonce, key_version \
                     FROM mcp_server_credentials",
                )
                .fetch_all(&mut **tx)
                .await?)
            })
            .await?;
        rows.into_iter().map(|r| r.into_record(&self.enc)).collect()
    }

    async fn read(
        &self,
        server_id: McpServerId,
        org_id: OrgId,
    ) -> Result<Option<McpCredentialRecord>, McpError> {
        let row = crate::auth::run_privileged::<Option<CredentialRow>, McpError>(
            &self.pool,
            async |tx| {
                Ok(sqlx::query_as::<_, CredentialRow>(
                    "SELECT server_id, org_id, kind, ciphertext, nonce, key_version \
                     FROM mcp_server_credentials WHERE server_id = $1 AND org_id = $2",
                )
                .bind(server_id)
                .bind(org_id)
                .fetch_optional(&mut **tx)
                .await?)
            },
        )
        .await?;
        row.map(|r| r.into_record(&self.enc)).transpose()
    }
}

#[derive(sqlx::FromRow)]
struct CredentialRow {
    server_id: McpServerId,
    org_id: OrgId,
    kind: String,
    ciphertext: Vec<u8>,
    nonce: Vec<u8>,
    key_version: i16,
}

impl CredentialRow {
    fn into_record(self, enc: &SharedOrgEncryptor) -> Result<McpCredentialRecord, McpError> {
        // The DB CHECK guarantees `nonce` is exactly 12 bytes. Defence in
        // depth: refuse to proceed if a future schema drift somehow lets a
        // wrong-length nonce in (CLAUDE.md §6).
        let nonce_arr: [u8; crate::crypto::NONCE_BYTES] = self
            .nonce
            .as_slice()
            .try_into()
            .map_err(|_| McpError::Backend("credential nonce: wrong byte length".into()))?;
        let blob = EncryptedBlob {
            key_version: self.key_version,
            nonce: nonce_arr,
            ciphertext: self.ciphertext,
        };
        let payload = open_payload(enc, self.org_id, self.kind.as_str(), &blob)?;
        Ok(McpCredentialRecord {
            server_id: self.server_id,
            org_id: self.org_id,
            payload,
        })
    }
}
