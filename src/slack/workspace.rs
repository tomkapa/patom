//! Per-org Slack workspace install — `slack_workspaces` table.
//!
//! `read_by_team` is the webhook path: privileged (no Principal yet),
//! resolves a `team_id` to its installed workspace and decrypts the bot
//! token via `OrgEncryptor` before returning. Every other operation
//! runs under `begin_as_user` so RLS prevents cross-tenant writes.

use std::fmt;
use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sqlx::PgPool;

use crate::auth::{OrgId, Principal, UserId, run_as_user, run_privileged};
use crate::clock::SharedClock;
use crate::crypto::{EncryptedBlob, SharedOrgEncryptor};

use super::error::SlackError;
use super::types::{SlackBotToken, SlackTeamId, SlackUserId};

/// What we hand the OAuth callback: a fresh workspace install.
#[derive(Debug)]
pub struct NewWorkspace {
    pub org_id: OrgId,
    pub team_id: SlackTeamId,
    pub team_name: String,
    pub bot_user_id: SlackUserId,
    pub bot_token: SlackBotToken,
    pub scopes: String,
    pub installed_by_user_id: UserId,
}

/// Webhook-side projection — already decrypted.
#[derive(Debug, Clone)]
pub struct WorkspaceWithToken {
    pub org_id: OrgId,
    pub team_id: SlackTeamId,
    pub bot_user_id: SlackUserId,
    pub bot_token: SlackBotToken,
    pub installed_by_user_id: UserId,
    pub installed_at: DateTime<Utc>,
}

#[async_trait]
pub trait SlackWorkspaceStore: fmt::Debug + Send + Sync {
    /// Install or replace a workspace row. Caller-driven (user-facing
    /// OAuth callback); runs `begin_as_user` so RLS allows the write
    /// only if `principal` is a member of `new.org_id`.
    async fn upsert(&self, principal: &Principal, new: NewWorkspace) -> Result<(), SlackError>;

    /// Resolve a Slack `team_id` to its workspace, decrypting the bot
    /// token in the process. Runs `begin_privileged` because the
    /// webhook arrives before any `Principal` is known.
    async fn read_by_team(&self, team_id: &SlackTeamId) -> Result<WorkspaceWithToken, SlackError>;

    /// Tenant-scoped uninstall. ON DELETE CASCADE on
    /// `slack_workspaces` cleans up identities + threads.
    async fn delete(&self, principal: &Principal, team_id: &SlackTeamId) -> Result<(), SlackError>;
}

pub type SharedSlackWorkspaceStore = Arc<dyn SlackWorkspaceStore>;

pub struct PgSlackWorkspaceStore {
    pool: PgPool,
    clock: SharedClock,
    enc: SharedOrgEncryptor,
}

impl PgSlackWorkspaceStore {
    #[must_use]
    pub fn new(pool: PgPool, clock: SharedClock, enc: SharedOrgEncryptor) -> Self {
        Self { pool, clock, enc }
    }
}

impl fmt::Debug for PgSlackWorkspaceStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PgSlackWorkspaceStore")
            .finish_non_exhaustive()
    }
}

const SQL_UPSERT: &str = "INSERT INTO slack_workspaces \
     (org_id, team_id, team_name, bot_user_id, bot_token_ciphertext, \
      bot_token_nonce, key_version, scopes, installed_by_user_id, installed_at) \
     VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10) \
     ON CONFLICT (org_id, team_id) DO UPDATE SET \
        team_name = EXCLUDED.team_name, \
        bot_user_id = EXCLUDED.bot_user_id, \
        bot_token_ciphertext = EXCLUDED.bot_token_ciphertext, \
        bot_token_nonce = EXCLUDED.bot_token_nonce, \
        key_version = EXCLUDED.key_version, \
        scopes = EXCLUDED.scopes, \
        installed_by_user_id = EXCLUDED.installed_by_user_id, \
        installed_at = EXCLUDED.installed_at";

const SQL_READ_BY_TEAM: &str = "SELECT \
        org_id, team_id, bot_user_id, bot_token_ciphertext, \
        bot_token_nonce, key_version, installed_by_user_id, installed_at \
     FROM slack_workspaces WHERE team_id = $1";

#[async_trait]
impl SlackWorkspaceStore for PgSlackWorkspaceStore {
    async fn upsert(&self, principal: &Principal, new: NewWorkspace) -> Result<(), SlackError> {
        // RLS: enforce that principal belongs to new.org_id. The
        // policy on slack_workspaces requires `app_user_is_member(org_id)`
        // which `run_as_user` arranges via the `app.user_id` GUC.
        assert_eq!(
            principal.active_org_id, new.org_id,
            "invariant: workspace install org must match active org"
        );
        let blob = self
            .enc
            .seal(new.org_id, new.bot_token.expose().as_bytes())?;
        let now = self.clock.now_utc();
        let user_id = principal.user_id;
        run_as_user::<(), SlackError>(&self.pool, user_id, async |tx| {
            sqlx::query(SQL_UPSERT)
                .bind(new.org_id)
                .bind(new.team_id.as_str())
                .bind(&new.team_name)
                .bind(new.bot_user_id.as_str())
                .bind(&blob.ciphertext)
                .bind(blob.nonce.as_slice())
                .bind(blob.key_version)
                .bind(&new.scopes)
                .bind(new.installed_by_user_id)
                .bind(now)
                .execute(&mut **tx)
                .await?;
            Ok(())
        })
        .await
    }

    async fn read_by_team(&self, team_id: &SlackTeamId) -> Result<WorkspaceWithToken, SlackError> {
        type Row = (
            OrgId,
            String,
            String,
            Vec<u8>,
            Vec<u8>,
            i16,
            UserId,
            DateTime<Utc>,
        );
        let row: Option<Row> = run_privileged::<Option<Row>, SlackError>(&self.pool, async |tx| {
            Ok(sqlx::query_as(SQL_READ_BY_TEAM)
                .bind(team_id.as_str())
                .fetch_optional(&mut **tx)
                .await?)
        })
        .await?;
        let (
            org_id,
            team_id_str,
            bot_user_id_str,
            ciphertext,
            nonce_vec,
            key_version,
            installed_by_user_id,
            installed_at,
        ) = row.ok_or_else(|| SlackError::UnknownWorkspace(team_id.as_str().to_owned()))?;

        // Defensive: nonce is fixed-size at the schema level.
        let mut nonce = [0u8; 12];
        if nonce_vec.len() != nonce.len() {
            return Err(SlackError::Internal(format!(
                "slack_workspaces.bot_token_nonce wrong length: got {}",
                nonce_vec.len()
            )));
        }
        nonce.copy_from_slice(&nonce_vec);
        let blob = EncryptedBlob {
            key_version,
            nonce,
            ciphertext,
        };
        let plain = self.enc.open(org_id, &blob)?;
        let token_str = std::str::from_utf8(plain.as_slice())
            .map_err(|_| SlackError::Internal("bot token decrypts to invalid utf-8".to_owned()))?
            .to_owned();
        // Re-validate via the newtype so the loaded value satisfies the
        // same invariants as a freshly-installed token.
        let bot_token = SlackBotToken::try_from(token_str)?;
        let team_id_typed = SlackTeamId::try_from(team_id_str)?;
        let bot_user_id_typed = SlackUserId::try_from(bot_user_id_str)?;
        Ok(WorkspaceWithToken {
            org_id,
            team_id: team_id_typed,
            bot_user_id: bot_user_id_typed,
            bot_token,
            installed_by_user_id,
            installed_at,
        })
    }

    async fn delete(&self, principal: &Principal, team_id: &SlackTeamId) -> Result<(), SlackError> {
        let user_id = principal.user_id;
        run_as_user::<(), SlackError>(&self.pool, user_id, async |tx| {
            sqlx::query("DELETE FROM slack_workspaces WHERE org_id = $1 AND team_id = $2")
                .bind(principal.active_org_id)
                .bind(team_id.as_str())
                .execute(&mut **tx)
                .await?;
            Ok(())
        })
        .await
    }
}
