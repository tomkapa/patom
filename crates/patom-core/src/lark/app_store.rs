//! Per-org self-built Lark app registration — the `lark_apps` table.
//!
//! Each customer registers one self-built ("internal") Lark app per agent;
//! this store owns the `(org_id, app_id) -> agent_id` mapping and the
//! encrypted `app_secret`. The admin-facing operations ([`LarkAppStore::register`],
//! [`LarkAppStore::list`], [`LarkAppStore::delete`]) run under `run_as_user` so
//! RLS (`app_user_is_member(org_id)`) prevents cross-tenant writes. The
//! bridge-facing operations ([`LarkAppStore::read_by_app_id`],
//! [`LarkAppStore::list_connect_targets`], [`LarkAppStore::set_tenant_key`]) and
//! the [`AppSecretSource`] impl run `run_privileged`, because the WS manager and
//! token provider act before any `Caller` is known. The secret is sealed via
//! [`SharedOrgEncryptor`] on write and re-opened on read; the plaintext never
//! reaches the settings-listing path.

use std::fmt;
use std::sync::Arc;

use async_trait::async_trait;
use sqlx::PgPool;

use crate::agents::AgentId;
use crate::auth::{Caller, OrgId, run_as_user, run_privileged};
use crate::clock::SharedClock;
use crate::crypto::{EncryptedBlob, SharedOrgEncryptor};

use super::error::LarkError;
use super::token::AppSecretSource;
use super::types::{LarkAppId, LarkAppSecret, TenantKey};

/// What the admin registration route hands us: a fresh app install.
///
/// `app_secret` is plaintext here and only here; it is sealed before it
/// touches the database and never read back into this shape.
#[derive(Debug, Clone)]
pub struct NewLarkApp {
    pub app_id: LarkAppId,
    pub agent_id: AgentId,
    pub app_secret: LarkAppSecret,
}

/// Registration projection — the secret is intentionally absent.
///
/// Powers the settings listing and the bridge's app-resolution path. The
/// `tenant_key` is `None` until the first inbound event for the app lets us
/// learn it (see [`LarkAppStore::set_tenant_key`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LarkApp {
    pub org_id: OrgId,
    pub app_id: LarkAppId,
    pub agent_id: AgentId,
    pub tenant_key: Option<TenantKey>,
}

/// The minimal `(org_id, app_id)` pair the WS manager needs to open a
/// long-connection for every registered bot at startup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LarkConnectTarget {
    pub org_id: OrgId,
    pub app_id: LarkAppId,
}

#[async_trait]
pub trait LarkAppStore: fmt::Debug + Send + Sync {
    /// Register (or replace) a self-built Lark app for the caller's org.
    ///
    /// Runs `run_as_user(caller)` so RLS permits the write only when the
    /// caller is a member of `caller.org_id`. The secret is sealed before
    /// binding; on `(org_id, app_id)` conflict the secret and `agent_id`
    /// are updated in place.
    async fn register(&self, caller: &Caller, app: NewLarkApp) -> Result<(), LarkError>;

    /// List the apps registered to the caller's org (no secrets).
    ///
    /// RLS scopes the SELECT to the caller's org membership; the settings
    /// tab renders the result.
    async fn list(&self, caller: &Caller) -> Result<Vec<LarkApp>, LarkError>;

    /// Tenant-scoped deregistration of a single app.
    ///
    /// Returns [`LarkError::UnknownApp`] when no row matched — RLS plus the
    /// `org_id` predicate make "no match" indistinguishable from "not
    /// visible", so the handler maps both to 404.
    async fn delete(&self, caller: &Caller, app_id: &LarkAppId) -> Result<(), LarkError>;

    /// Resolve an `app_id` to its registration (no secret).
    ///
    /// Runs `run_privileged` because the inbound WS frame arrives before any
    /// `Caller` is known. Returns [`LarkError::UnknownApp`] when absent.
    async fn read_by_app_id(&self, app_id: &LarkAppId) -> Result<LarkApp, LarkError>;

    /// Reverse lookup: the bot (`app_id`) that speaks as `agent_id` in `org`,
    /// or `None` if that agent has no Lark bot.
    ///
    /// The outbound pump uses this so each reply in a multi-bot thread posts via
    /// the *replying* agent's own bot, not whichever bot first attached the pump.
    /// Privileged (the pump has no `Caller`).
    async fn app_id_for_agent(
        &self,
        org_id: OrgId,
        agent_id: AgentId,
    ) -> Result<Option<LarkAppId>, LarkError>;

    /// Every registered app as an `(org_id, app_id)` connect target.
    ///
    /// Runs `run_privileged`; the WS manager opens one long-connection per
    /// returned target at startup.
    async fn list_connect_targets(&self) -> Result<Vec<LarkConnectTarget>, LarkError>;

    /// Record the `tenant_key` learned from the first inbound event for an app.
    ///
    /// Runs `run_privileged` (the bridge has no `Caller`); idempotent —
    /// re-recording the same key is a no-op UPDATE.
    async fn set_tenant_key(
        &self,
        app_id: &LarkAppId,
        tenant_key: &TenantKey,
    ) -> Result<(), LarkError>;
}

pub type SharedLarkAppStore = Arc<dyn LarkAppStore>;

/// Postgres-backed [`LarkAppStore`] over the `lark_apps` table.
pub struct PgLarkAppStore {
    pool: PgPool,
    clock: SharedClock,
    enc: SharedOrgEncryptor,
}

impl PgLarkAppStore {
    #[must_use]
    pub fn new(pool: PgPool, clock: SharedClock, enc: SharedOrgEncryptor) -> Self {
        Self { pool, clock, enc }
    }
}

impl fmt::Debug for PgLarkAppStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PgLarkAppStore").finish_non_exhaustive()
    }
}

/// SELECT cap one above [`LIST_MAX_ROWS`] so a runaway RLS policy can never
/// let the in-handler assertion panic the process — the DB clips the result
/// before we count it.
const SQL_LIST: &str = "SELECT org_id, app_id, agent_id, tenant_key \
     FROM lark_apps \
     ORDER BY created_at DESC \
     LIMIT 1025";

/// SELECT cap one above [`CONNECT_TARGETS_MAX_ROWS`] (privileged, all-org).
const SQL_LIST_CONNECT_TARGETS: &str = "SELECT org_id, app_id \
     FROM lark_apps \
     ORDER BY created_at ASC \
     LIMIT 4097";

/// Loop bound for [`LarkAppStore::list`]. CLAUDE.md §5: hand-pick a
/// pessimistic cap and expose it so the SQL `LIMIT` and the assertion can't
/// drift. An org with >1024 self-built Lark apps is itself a bug.
const LIST_MAX_ROWS: usize = 1024;

/// Loop bound for [`LarkAppStore::list_connect_targets`]. Privileged, so this
/// counts every registered app across all orgs; sized for the whole fleet.
const CONNECT_TARGETS_MAX_ROWS: usize = 4096;

/// Reconstruct an [`EncryptedBlob`] from the raw column triple.
///
/// The nonce is a fixed-size 12-byte BYTEA at the schema level; a wrong
/// length means a corrupt row, which is an invariant violation, not an
/// expected failure.
fn blob_from_columns(
    ciphertext: Vec<u8>,
    nonce_vec: &[u8],
    key_version: i16,
) -> Result<EncryptedBlob, LarkError> {
    let mut nonce = [0u8; 12];
    if nonce_vec.len() != nonce.len() {
        return Err(LarkError::Internal(format!(
            "lark_apps.app_secret_nonce wrong length: got {}",
            nonce_vec.len()
        )));
    }
    nonce.copy_from_slice(nonce_vec);
    Ok(EncryptedBlob {
        key_version,
        nonce,
        ciphertext,
    })
}

#[async_trait]
impl LarkAppStore for PgLarkAppStore {
    async fn register(&self, caller: &Caller, app: NewLarkApp) -> Result<(), LarkError> {
        let blob = self
            .enc
            .seal(caller.org_id, app.app_secret.expose().as_bytes())?;
        let now = self.clock.now_utc();
        let org_id = caller.org_id;
        let user_id = caller.user_id;
        run_as_user::<(), LarkError>(&self.pool, user_id, async |tx| {
            sqlx::query(
                "INSERT INTO lark_apps \
                     (org_id, app_id, agent_id, app_secret_ciphertext, \
                      app_secret_nonce, key_version, created_at) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7) \
                 ON CONFLICT (org_id, app_id) DO UPDATE SET \
                     agent_id = EXCLUDED.agent_id, \
                     app_secret_ciphertext = EXCLUDED.app_secret_ciphertext, \
                     app_secret_nonce = EXCLUDED.app_secret_nonce, \
                     key_version = EXCLUDED.key_version",
            )
            .bind(org_id)
            .bind(app.app_id.as_str())
            .bind(app.agent_id)
            .bind(&blob.ciphertext)
            .bind(blob.nonce.as_slice())
            .bind(blob.key_version)
            .bind(now)
            .execute(&mut **tx)
            .await?;
            Ok(())
        })
        .await
    }

    async fn list(&self, caller: &Caller) -> Result<Vec<LarkApp>, LarkError> {
        // RLS restricts the SELECT to the caller's org membership; the query
        // carries `LIMIT LIST_MAX_ROWS + 1` so the assertion catches an
        // unbounded result before the loop runs (CLAUDE.md §5).
        type Row = (OrgId, String, AgentId, Option<String>);
        let rows: Vec<Row> =
            run_as_user::<Vec<Row>, LarkError>(&self.pool, caller.user_id, async |tx| {
                Ok(sqlx::query_as(SQL_LIST).fetch_all(&mut **tx).await?)
            })
            .await?;
        assert!(
            rows.len() <= LIST_MAX_ROWS,
            "invariant: lark app list bounded (got {}, max {})",
            rows.len(),
            LIST_MAX_ROWS,
        );
        let mut out = Vec::with_capacity(rows.len());
        for (org_id, app_id_str, agent_id, tenant_key_str) in rows {
            let app_id = LarkAppId::try_from(app_id_str)?;
            let tenant_key = tenant_key_str.map(TenantKey::try_from).transpose()?;
            out.push(LarkApp {
                org_id,
                app_id,
                agent_id,
                tenant_key,
            });
        }
        Ok(out)
    }

    async fn delete(&self, caller: &Caller, app_id: &LarkAppId) -> Result<(), LarkError> {
        let org_id = caller.org_id;
        let user_id = caller.user_id;
        let rows_affected = run_as_user::<u64, LarkError>(&self.pool, user_id, async |tx| {
            let res = sqlx::query("DELETE FROM lark_apps WHERE org_id = $1 AND app_id = $2")
                .bind(org_id)
                .bind(app_id.as_str())
                .execute(&mut **tx)
                .await?;
            Ok(res.rows_affected())
        })
        .await?;
        if rows_affected == 0 {
            return Err(LarkError::UnknownApp(app_id.clone()));
        }
        Ok(())
    }

    async fn read_by_app_id(&self, app_id: &LarkAppId) -> Result<LarkApp, LarkError> {
        type Row = (OrgId, String, AgentId, Option<String>);
        let row: Option<Row> = run_privileged::<Option<Row>, LarkError>(&self.pool, async |tx| {
            Ok(sqlx::query_as(
                "SELECT org_id, app_id, agent_id, tenant_key \
                 FROM lark_apps WHERE app_id = $1",
            )
            .bind(app_id.as_str())
            .fetch_optional(&mut **tx)
            .await?)
        })
        .await?;
        let (org_id, app_id_str, agent_id, tenant_key_str) =
            row.ok_or_else(|| LarkError::UnknownApp(app_id.clone()))?;
        let parsed_app_id = LarkAppId::try_from(app_id_str)?;
        let tenant_key = tenant_key_str.map(TenantKey::try_from).transpose()?;
        Ok(LarkApp {
            org_id,
            app_id: parsed_app_id,
            agent_id,
            tenant_key,
        })
    }

    async fn app_id_for_agent(
        &self,
        org_id: OrgId,
        agent_id: AgentId,
    ) -> Result<Option<LarkAppId>, LarkError> {
        let row: Option<(String,)> =
            run_privileged::<Option<(String,)>, LarkError>(&self.pool, async move |tx| {
                Ok(sqlx::query_as(
                    "SELECT app_id FROM lark_apps \
                     WHERE org_id = $1 AND agent_id = $2 LIMIT 1",
                )
                .bind(org_id)
                .bind(agent_id)
                .fetch_optional(&mut **tx)
                .await?)
            })
            .await?;
        match row {
            Some((app_id,)) => Ok(Some(LarkAppId::try_from(app_id)?)),
            None => Ok(None),
        }
    }

    async fn list_connect_targets(&self) -> Result<Vec<LarkConnectTarget>, LarkError> {
        type Row = (OrgId, String);
        let rows: Vec<Row> = run_privileged::<Vec<Row>, LarkError>(&self.pool, async |tx| {
            Ok(sqlx::query_as(SQL_LIST_CONNECT_TARGETS)
                .fetch_all(&mut **tx)
                .await?)
        })
        .await?;
        assert!(
            rows.len() <= CONNECT_TARGETS_MAX_ROWS,
            "invariant: lark connect-target list bounded (got {}, max {})",
            rows.len(),
            CONNECT_TARGETS_MAX_ROWS,
        );
        let mut out = Vec::with_capacity(rows.len());
        for (org_id, app_id_str) in rows {
            let app_id = LarkAppId::try_from(app_id_str)?;
            out.push(LarkConnectTarget { org_id, app_id });
        }
        Ok(out)
    }

    async fn set_tenant_key(
        &self,
        app_id: &LarkAppId,
        tenant_key: &TenantKey,
    ) -> Result<(), LarkError> {
        let rows_affected = run_privileged::<u64, LarkError>(&self.pool, async |tx| {
            let res = sqlx::query("UPDATE lark_apps SET tenant_key = $1 WHERE app_id = $2")
                .bind(tenant_key.as_str())
                .bind(app_id.as_str())
                .execute(&mut **tx)
                .await?;
            Ok(res.rows_affected())
        })
        .await?;
        if rows_affected == 0 {
            return Err(LarkError::UnknownApp(app_id.clone()));
        }
        Ok(())
    }
}

#[async_trait]
impl AppSecretSource for PgLarkAppStore {
    async fn secret(&self, app_id: &LarkAppId) -> Result<LarkAppSecret, LarkError> {
        type Row = (OrgId, Vec<u8>, Vec<u8>, i16);
        let row: Option<Row> = run_privileged::<Option<Row>, LarkError>(&self.pool, async |tx| {
            Ok(sqlx::query_as(
                "SELECT org_id, app_secret_ciphertext, app_secret_nonce, key_version \
                 FROM lark_apps WHERE app_id = $1",
            )
            .bind(app_id.as_str())
            .fetch_optional(&mut **tx)
            .await?)
        })
        .await?;
        let (org_id, ciphertext, nonce_vec, key_version) =
            row.ok_or_else(|| LarkError::UnknownApp(app_id.clone()))?;
        let blob = blob_from_columns(ciphertext, &nonce_vec, key_version)?;
        let plain = self.enc.open(org_id, &blob)?;
        let secret_str = std::str::from_utf8(plain.as_slice())
            .map_err(|_| LarkError::Internal("app secret decrypts to invalid utf-8".to_owned()))?
            .to_owned();
        // Re-validate via the newtype so the loaded value satisfies the same
        // invariants as a freshly-registered secret.
        Ok(LarkAppSecret::try_from(secret_str)?)
    }
}
