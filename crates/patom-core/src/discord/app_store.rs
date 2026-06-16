//! Per-org Discord app registration — the `discord_apps` table.
//!
//! Each customer registers one self-built Discord application per agent; this
//! store owns the `(org_id, application_id) -> agent_id` mapping and the
//! encrypted bot token. The admin-facing operations ([`DiscordAppStore::register`],
//! [`DiscordAppStore::list`], [`DiscordAppStore::delete`]) run under `run_as_user`
//! so RLS (`app_user_is_member(org_id)`) prevents cross-tenant writes. The
//! gateway/poster-facing operations ([`DiscordAppStore::read_by_app_id`],
//! [`DiscordAppStore::list_connect_targets`], [`DiscordAppStore::set_bot_user_id`])
//! and the [`BotTokenSource`] impl run `run_privileged`, because the gateway
//! manager and poster act before any `Caller` is known.
//!
//! Unlike Lark's expiring `tenant_access_token`, a Discord bot token is
//! **static** — there is no refresh loop, no token cache. The token is sealed via
//! [`SharedOrgEncryptor`] on write and re-opened on demand. A 401 (a reset or
//! leaked token) is not refreshable: it surfaces as a typed error, and the admin
//! re-credentials by re-`register`-ing (the upsert path), which is the rotation.

use std::fmt;
use std::sync::Arc;

use async_trait::async_trait;
use sqlx::PgPool;

use crate::agents::AgentId;
use crate::auth::{Caller, OrgId, run_as_user, run_privileged};
use crate::clock::SharedClock;
use crate::crypto::{EncryptedBlob, SharedOrgEncryptor};

use super::error::DiscordError;
use super::types::{ApplicationId, BotToken, DiscordUserId};

/// What the admin registration route hands us: a fresh app install.
///
/// `bot_token` is plaintext here and only here; it is sealed before it touches
/// the database and never read back into this shape.
#[derive(Debug, Clone)]
pub struct NewDiscordApp {
    pub application_id: ApplicationId,
    pub agent_id: AgentId,
    pub bot_token: BotToken,
}

/// Registration projection — the token is intentionally absent.
///
/// Powers the settings listing and the bridge's app-resolution path. The
/// `bot_user_id` is `None` until the first `READY` lets us learn it (see
/// [`DiscordAppStore::set_bot_user_id`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscordApp {
    pub org_id: OrgId,
    pub application_id: ApplicationId,
    pub agent_id: AgentId,
    pub bot_user_id: Option<DiscordUserId>,
}

/// The minimal `(org_id, application_id)` pair the gateway manager needs to open
/// a connection for every registered bot at startup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscordConnectTarget {
    pub org_id: OrgId,
    pub application_id: ApplicationId,
}

/// Loop bound for [`DiscordAppStore::list`]. CLAUDE.md §5: hand-pick a
/// pessimistic cap and expose it so the SQL `LIMIT` and the assertion can't
/// drift. An org with >1024 self-built Discord apps is itself a bug.
const LIST_MAX_ROWS: usize = 1024;

/// Loop bound for [`DiscordAppStore::list_connect_targets`] (privileged, fleet-wide).
const CONNECT_TARGETS_MAX_ROWS: usize = 4096;

/// SELECT cap one above [`LIST_MAX_ROWS`] so a runaway RLS policy can never let
/// the in-handler assertion panic the process — the DB clips first.
const SQL_LIST: &str = "SELECT org_id, application_id, agent_id, bot_user_id \
     FROM discord_apps \
     ORDER BY created_at DESC \
     LIMIT 1025";

/// SELECT cap one above [`CONNECT_TARGETS_MAX_ROWS`] (privileged, all-org).
const SQL_LIST_CONNECT_TARGETS: &str = "SELECT org_id, application_id \
     FROM discord_apps \
     ORDER BY created_at ASC \
     LIMIT 4097";

#[async_trait]
pub trait DiscordAppStore: fmt::Debug + Send + Sync {
    /// Register (or replace) a self-built Discord app for the caller's org.
    ///
    /// Runs `run_as_user(caller)` so RLS permits the write only when the caller
    /// is a member of `caller.org_id`. The token is sealed before binding; on
    /// `(org_id, application_id)` conflict the token and `agent_id` update in
    /// place (the rotation path).
    async fn register(&self, caller: &Caller, app: NewDiscordApp) -> Result<(), DiscordError>;

    /// List the apps registered to the caller's org (no tokens).
    async fn list(&self, caller: &Caller) -> Result<Vec<DiscordApp>, DiscordError>;

    /// Tenant-scoped deregistration of a single app. Returns
    /// [`DiscordError::UnknownApp`] when no row matched (RLS makes "no match"
    /// and "not visible" identical — both map to 404).
    async fn delete(&self, caller: &Caller, app_id: &ApplicationId) -> Result<(), DiscordError>;

    /// Resolve an `application_id` to its registration (no token). Runs
    /// `run_privileged` (the inbound event arrives before any `Caller`).
    async fn read_by_app_id(&self, app_id: &ApplicationId) -> Result<DiscordApp, DiscordError>;

    /// Reverse lookup: the bot (`application_id`) that speaks as `agent_id` in
    /// `org`, or `None`. The outbound router uses this so each reply posts via
    /// the *replying* agent's own bot. Privileged.
    async fn app_id_for_agent(
        &self,
        org_id: OrgId,
        agent_id: AgentId,
    ) -> Result<Option<ApplicationId>, DiscordError>;

    /// Every registered app as a connect target. Privileged; the gateway manager
    /// opens one connection per returned target at startup.
    async fn list_connect_targets(&self) -> Result<Vec<DiscordConnectTarget>, DiscordError>;

    /// Record the `bot_user_id` learned from the first `READY` for an app. Runs
    /// `run_privileged` (the gateway has no `Caller`); idempotent.
    async fn set_bot_user_id(
        &self,
        app_id: &ApplicationId,
        bot_user_id: &DiscordUserId,
    ) -> Result<(), DiscordError>;
}

pub type SharedDiscordAppStore = Arc<dyn DiscordAppStore>;

/// The static-token seam: the decrypted [`BotToken`] for an `application_id`.
///
/// The gateway manager (IDENTIFY) and the poster (`Authorization: Bot …`) both
/// depend on this, not on the concrete store. No cache/refresh — the token is
/// static, decrypted on demand.
#[async_trait]
pub trait BotTokenSource: fmt::Debug + Send + Sync {
    async fn token(&self, app_id: &ApplicationId) -> Result<BotToken, DiscordError>;
}

pub type SharedBotTokenSource = Arc<dyn BotTokenSource>;

/// Postgres-backed [`DiscordAppStore`] over the `discord_apps` table.
pub struct PgDiscordAppStore {
    pool: PgPool,
    clock: SharedClock,
    enc: SharedOrgEncryptor,
}

impl PgDiscordAppStore {
    #[must_use]
    pub fn new(pool: PgPool, clock: SharedClock, enc: SharedOrgEncryptor) -> Self {
        Self { pool, clock, enc }
    }
}

impl fmt::Debug for PgDiscordAppStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PgDiscordAppStore").finish_non_exhaustive()
    }
}

/// Reconstruct an [`EncryptedBlob`] from the raw column triple. A wrong nonce
/// length means a corrupt row — an invariant violation, not an expected failure.
fn blob_from_columns(
    ciphertext: Vec<u8>,
    nonce_vec: &[u8],
    key_version: i16,
) -> Result<EncryptedBlob, DiscordError> {
    let mut nonce = [0u8; 12];
    if nonce_vec.len() != nonce.len() {
        return Err(DiscordError::Internal(format!(
            "discord_apps.bot_token_nonce wrong length: got {}",
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

/// Parse a row's `(application_id, bot_user_id)` text columns into newtypes.
fn parse_app_row(
    org_id: OrgId,
    application_id: String,
    agent_id: AgentId,
    bot_user_id: Option<String>,
) -> Result<DiscordApp, DiscordError> {
    Ok(DiscordApp {
        org_id,
        application_id: ApplicationId::try_from(application_id)?,
        agent_id,
        bot_user_id: bot_user_id.map(DiscordUserId::try_from).transpose()?,
    })
}

#[async_trait]
impl DiscordAppStore for PgDiscordAppStore {
    async fn register(&self, caller: &Caller, app: NewDiscordApp) -> Result<(), DiscordError> {
        let blob = self
            .enc
            .seal(caller.org_id, app.bot_token.expose().as_bytes())?;
        let now = self.clock.now_utc();
        let org_id = caller.org_id;
        run_as_user::<(), DiscordError>(&self.pool, caller.user_id, async |tx| {
            sqlx::query(
                "INSERT INTO discord_apps \
                     (org_id, application_id, agent_id, bot_token_ciphertext, \
                      bot_token_nonce, key_version, created_at) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7) \
                 ON CONFLICT (org_id, application_id) DO UPDATE SET \
                     agent_id = EXCLUDED.agent_id, \
                     bot_token_ciphertext = EXCLUDED.bot_token_ciphertext, \
                     bot_token_nonce = EXCLUDED.bot_token_nonce, \
                     key_version = EXCLUDED.key_version",
            )
            .bind(org_id)
            .bind(app.application_id.as_str())
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

    async fn list(&self, caller: &Caller) -> Result<Vec<DiscordApp>, DiscordError> {
        type Row = (OrgId, String, AgentId, Option<String>);
        let rows: Vec<Row> =
            run_as_user::<Vec<Row>, DiscordError>(&self.pool, caller.user_id, async |tx| {
                Ok(sqlx::query_as(SQL_LIST).fetch_all(&mut **tx).await?)
            })
            .await?;
        assert!(
            rows.len() <= LIST_MAX_ROWS,
            "invariant: discord app list bounded (got {}, max {LIST_MAX_ROWS})",
            rows.len(),
        );
        let mut out = Vec::with_capacity(rows.len());
        for (org_id, application_id, agent_id, bot_user_id) in rows {
            out.push(parse_app_row(
                org_id,
                application_id,
                agent_id,
                bot_user_id,
            )?);
        }
        Ok(out)
    }

    async fn delete(&self, caller: &Caller, app_id: &ApplicationId) -> Result<(), DiscordError> {
        let org_id = caller.org_id;
        let rows_affected =
            run_as_user::<u64, DiscordError>(&self.pool, caller.user_id, async |tx| {
                let res = sqlx::query(
                    "DELETE FROM discord_apps WHERE org_id = $1 AND application_id = $2",
                )
                .bind(org_id)
                .bind(app_id.as_str())
                .execute(&mut **tx)
                .await?;
                Ok(res.rows_affected())
            })
            .await?;
        if rows_affected == 0 {
            return Err(DiscordError::UnknownApp(app_id.clone()));
        }
        Ok(())
    }

    async fn read_by_app_id(&self, app_id: &ApplicationId) -> Result<DiscordApp, DiscordError> {
        type Row = (OrgId, String, AgentId, Option<String>);
        let row: Option<Row> =
            run_privileged::<Option<Row>, DiscordError>(&self.pool, async |tx| {
                Ok(sqlx::query_as(
                    "SELECT org_id, application_id, agent_id, bot_user_id \
                     FROM discord_apps WHERE application_id = $1",
                )
                .bind(app_id.as_str())
                .fetch_optional(&mut **tx)
                .await?)
            })
            .await?;
        let (org_id, application_id, agent_id, bot_user_id) =
            row.ok_or_else(|| DiscordError::UnknownApp(app_id.clone()))?;
        parse_app_row(org_id, application_id, agent_id, bot_user_id)
    }

    async fn app_id_for_agent(
        &self,
        org_id: OrgId,
        agent_id: AgentId,
    ) -> Result<Option<ApplicationId>, DiscordError> {
        let row: Option<(String,)> =
            run_privileged::<Option<(String,)>, DiscordError>(&self.pool, async move |tx| {
                Ok(sqlx::query_as(
                    "SELECT application_id FROM discord_apps \
                     WHERE org_id = $1 AND agent_id = $2 LIMIT 1",
                )
                .bind(org_id)
                .bind(agent_id)
                .fetch_optional(&mut **tx)
                .await?)
            })
            .await?;
        match row {
            Some((application_id,)) => Ok(Some(ApplicationId::try_from(application_id)?)),
            None => Ok(None),
        }
    }

    async fn list_connect_targets(&self) -> Result<Vec<DiscordConnectTarget>, DiscordError> {
        type Row = (OrgId, String);
        let rows: Vec<Row> = run_privileged::<Vec<Row>, DiscordError>(&self.pool, async |tx| {
            Ok(sqlx::query_as(SQL_LIST_CONNECT_TARGETS)
                .fetch_all(&mut **tx)
                .await?)
        })
        .await?;
        assert!(
            rows.len() <= CONNECT_TARGETS_MAX_ROWS,
            "invariant: discord connect-target list bounded (got {}, max {CONNECT_TARGETS_MAX_ROWS})",
            rows.len(),
        );
        let mut out = Vec::with_capacity(rows.len());
        for (org_id, application_id) in rows {
            out.push(DiscordConnectTarget {
                org_id,
                application_id: ApplicationId::try_from(application_id)?,
            });
        }
        Ok(out)
    }

    async fn set_bot_user_id(
        &self,
        app_id: &ApplicationId,
        bot_user_id: &DiscordUserId,
    ) -> Result<(), DiscordError> {
        let rows_affected = run_privileged::<u64, DiscordError>(&self.pool, async |tx| {
            let res =
                sqlx::query("UPDATE discord_apps SET bot_user_id = $1 WHERE application_id = $2")
                    .bind(bot_user_id.as_str())
                    .bind(app_id.as_str())
                    .execute(&mut **tx)
                    .await?;
            Ok(res.rows_affected())
        })
        .await?;
        if rows_affected == 0 {
            return Err(DiscordError::UnknownApp(app_id.clone()));
        }
        Ok(())
    }
}

#[async_trait]
impl BotTokenSource for PgDiscordAppStore {
    async fn token(&self, app_id: &ApplicationId) -> Result<BotToken, DiscordError> {
        type Row = (OrgId, Vec<u8>, Vec<u8>, i16);
        let row: Option<Row> =
            run_privileged::<Option<Row>, DiscordError>(&self.pool, async |tx| {
                Ok(sqlx::query_as(
                    "SELECT org_id, bot_token_ciphertext, bot_token_nonce, key_version \
                     FROM discord_apps WHERE application_id = $1",
                )
                .bind(app_id.as_str())
                .fetch_optional(&mut **tx)
                .await?)
            })
            .await?;
        let (org_id, ciphertext, nonce_vec, key_version) =
            row.ok_or_else(|| DiscordError::UnknownApp(app_id.clone()))?;
        let blob = blob_from_columns(ciphertext, &nonce_vec, key_version)?;
        let plain = self.enc.open(org_id, &blob)?;
        let token_str = std::str::from_utf8(plain.as_slice())
            .map_err(|_| DiscordError::Internal("bot token decrypts to invalid utf-8".to_owned()))?
            .to_owned();
        // Re-validate via the newtype so the loaded value satisfies the same
        // invariants as a freshly-registered token.
        Ok(BotToken::try_from(token_str)?)
    }
}
