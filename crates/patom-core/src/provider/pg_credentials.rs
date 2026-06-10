//! Postgres-backed [`OrgProviderCredentialStore`].
//!
//! Holds a handle to the process-wide [`OrgEncryptor`]. Every seal/open happens
//! inside this store; nothing above it ever sees raw ciphertext (callers
//! operate on the typed [`ProviderApiKey`]). Mirrors `mcp/pg_credentials.rs`.

use std::fmt;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sqlx::PgPool;

use crate::auth::OrgId;
use crate::clock::SharedClock;
use crate::crypto::{EncryptedBlob, SharedOrgEncryptor};

use super::catalog::Model;
use super::credentials::{
    OrgProviderCredentialStore, ProviderBaseUrl, ProviderCredentialError, ProviderCredentialRecord,
    ProviderCredentialWrite, open_key, seal_key,
};
use super::id::ProviderId;

pub struct PgOrgProviderCredentialStore {
    pool: PgPool,
    clock: SharedClock,
    enc: SharedOrgEncryptor,
}

impl PgOrgProviderCredentialStore {
    #[must_use]
    pub fn new(pool: PgPool, clock: SharedClock, enc: SharedOrgEncryptor) -> Self {
        Self { pool, clock, enc }
    }
}

impl fmt::Debug for PgOrgProviderCredentialStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PgOrgProviderCredentialStore")
            .finish_non_exhaustive()
    }
}

#[async_trait]
impl OrgProviderCredentialStore for PgOrgProviderCredentialStore {
    async fn upsert(&self, write: ProviderCredentialWrite) -> Result<(), ProviderCredentialError> {
        let ProviderCredentialWrite {
            org_id,
            provider,
            api_key,
            base_url,
        } = write;
        let blob = seal_key(&self.enc, org_id, &api_key)?;
        let base_url_str = base_url.map(ProviderBaseUrl::into_string);
        let now = self.clock.now_utc();
        // Replacing the key resets validation state: the new secret has not yet
        // been tested against the provider (a later `mark_validated` stamps it).
        crate::auth::run_privileged::<(), ProviderCredentialError>(&self.pool, async |tx| {
            sqlx::query(
                "INSERT INTO org_provider_credentials \
                 (org_id, provider, ciphertext, nonce, key_version, base_url, \
                  last_validated_at, created_at, updated_at) \
                 VALUES ($1, $2, $3, $4, $5, $6, NULL, $7, $7) \
                 ON CONFLICT (org_id, provider) DO UPDATE SET \
                     ciphertext = EXCLUDED.ciphertext, \
                     nonce = EXCLUDED.nonce, \
                     key_version = EXCLUDED.key_version, \
                     base_url = EXCLUDED.base_url, \
                     last_validated_at = NULL, \
                     updated_at = EXCLUDED.updated_at",
            )
            .bind(org_id)
            .bind(provider)
            .bind(&blob.ciphertext)
            .bind(&blob.nonce[..])
            .bind(blob.key_version)
            .bind(base_url_str)
            .bind(now)
            .execute(&mut **tx)
            .await?;
            Ok(())
        })
        .await
    }

    async fn delete(
        &self,
        org_id: OrgId,
        provider: ProviderId,
    ) -> Result<(), ProviderCredentialError> {
        crate::auth::run_privileged::<(), ProviderCredentialError>(&self.pool, async |tx| {
            sqlx::query("DELETE FROM org_provider_credentials WHERE org_id = $1 AND provider = $2")
                .bind(org_id)
                .bind(provider)
                .execute(&mut **tx)
                .await?;
            Ok(())
        })
        .await
    }

    async fn list_all(&self) -> Result<Vec<ProviderCredentialRecord>, ProviderCredentialError> {
        let rows = crate::auth::run_privileged::<Vec<CredentialRow>, ProviderCredentialError>(
            &self.pool,
            async |tx| {
                Ok(sqlx::query_as::<_, CredentialRow>(
                    "SELECT org_id, provider, ciphertext, nonce, key_version, base_url, \
                     last_validated_at FROM org_provider_credentials",
                )
                .fetch_all(&mut **tx)
                .await?)
            },
        )
        .await?;
        rows.into_iter().map(|r| r.into_record(&self.enc)).collect()
    }

    async fn list_for_org(
        &self,
        org_id: OrgId,
    ) -> Result<Vec<ProviderCredentialRecord>, ProviderCredentialError> {
        let rows = crate::auth::run_privileged::<Vec<CredentialRow>, ProviderCredentialError>(
            &self.pool,
            async |tx| {
                Ok(sqlx::query_as::<_, CredentialRow>(
                    "SELECT org_id, provider, ciphertext, nonce, key_version, base_url, \
                     last_validated_at FROM org_provider_credentials WHERE org_id = $1",
                )
                .bind(org_id)
                .fetch_all(&mut **tx)
                .await?)
            },
        )
        .await?;
        rows.into_iter().map(|r| r.into_record(&self.enc)).collect()
    }

    async fn mark_validated(
        &self,
        org_id: OrgId,
        provider: ProviderId,
        now: DateTime<Utc>,
    ) -> Result<(), ProviderCredentialError> {
        crate::auth::run_privileged::<(), ProviderCredentialError>(&self.pool, async |tx| {
            sqlx::query(
                "UPDATE org_provider_credentials SET last_validated_at = $3, updated_at = $3 \
                 WHERE org_id = $1 AND provider = $2",
            )
            .bind(org_id)
            .bind(provider)
            .bind(now)
            .execute(&mut **tx)
            .await?;
            Ok(())
        })
        .await
    }

    async fn set_default_model(
        &self,
        org_id: OrgId,
        model: Model,
    ) -> Result<(), ProviderCredentialError> {
        crate::auth::run_privileged::<(), ProviderCredentialError>(&self.pool, async |tx| {
            sqlx::query("UPDATE organizations SET default_model = $2 WHERE id = $1")
                .bind(org_id)
                .bind(model.as_str())
                .execute(&mut **tx)
                .await?;
            Ok(())
        })
        .await
    }

    async fn list_default_models(&self) -> Result<Vec<(OrgId, Model)>, ProviderCredentialError> {
        let rows = crate::auth::run_privileged::<Vec<(OrgId, String)>, ProviderCredentialError>(
            &self.pool,
            async |tx| {
                Ok(sqlx::query_as::<_, (OrgId, String)>(
                    "SELECT id, default_model FROM organizations WHERE default_model IS NOT NULL",
                )
                .fetch_all(&mut **tx)
                .await?)
            },
        )
        .await?;
        // Skip (with a warn) any org whose stored model name no longer parses,
        // rather than failing the whole refresher (CLAUDE.md §4: one bad row
        // doesn't abort the batch).
        let mut out = Vec::with_capacity(rows.len());
        for (org_id, name) in rows {
            if let Ok(m) = Model::try_from(name.as_str()) {
                out.push((org_id, m));
            } else {
                tracing::warn!(
                    patom.org.id = %org_id,
                    "provider.overlay.default_model_unparseable",
                );
            }
        }
        Ok(out)
    }
}

#[derive(sqlx::FromRow)]
struct CredentialRow {
    org_id: OrgId,
    provider: ProviderId,
    ciphertext: Vec<u8>,
    nonce: Vec<u8>,
    key_version: i16,
    base_url: Option<String>,
    last_validated_at: Option<DateTime<Utc>>,
}

impl CredentialRow {
    fn into_record(
        self,
        enc: &SharedOrgEncryptor,
    ) -> Result<ProviderCredentialRecord, ProviderCredentialError> {
        // The DB CHECK guarantees a 12-byte nonce; refuse to proceed on drift
        // (CLAUDE.md §6).
        let nonce_arr: [u8; crate::crypto::NONCE_BYTES] = self
            .nonce
            .as_slice()
            .try_into()
            .map_err(|_| ProviderCredentialError::Corrupt("nonce wrong byte length"))?;
        let blob = EncryptedBlob {
            key_version: self.key_version,
            nonce: nonce_arr,
            ciphertext: self.ciphertext,
        };
        let api_key = open_key(enc, self.org_id, &blob)?;
        let base_url = self
            .base_url
            .map(ProviderBaseUrl::try_from)
            .transpose()
            .map_err(|_| ProviderCredentialError::Corrupt("base_url fails boundary parse"))?;
        Ok(ProviderCredentialRecord {
            org_id: self.org_id,
            provider: self.provider,
            api_key,
            base_url,
            last_validated_at: self.last_validated_at,
        })
    }
}
