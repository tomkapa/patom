//! Slack-user ↔ Relay-user link — `slack_identities` table.
//!
//! Phase 1 ships with no per-user linking: `lookup` falls back to the
//! workspace's `installed_by_user_id` whenever there's no explicit
//! `slack_identities` row, so the entire workspace effectively bridges
//! as the installer. Phase 2 (issue #41) wires up the DM-confirmation
//! flow that populates real rows here.

use std::fmt;
use std::sync::Arc;

use async_trait::async_trait;
use sqlx::PgPool;

use crate::auth::{OrgId, Principal, UserId, run_as_user, run_privileged};
use crate::clock::SharedClock;

use super::error::SlackError;
use super::types::{SlackTeamId, SlackUserId};

/// Resolved Relay-side identity for an inbound Slack event.
#[derive(Debug, Clone, Copy)]
pub struct LinkedIdentity {
    pub user_id: UserId,
    pub org_id: OrgId,
}

#[async_trait]
pub trait SlackIdentityStore: fmt::Debug + Send + Sync {
    /// Webhook-side lookup. Privileged because the request carries no
    /// Principal. Returns `Ok(Some(_))` for an explicit
    /// `slack_identities` row; `Ok(None)` for an unknown user (caller
    /// applies the workspace fallback). `Err` only on DB failures.
    async fn lookup(
        &self,
        team_id: &SlackTeamId,
        slack_user_id: &SlackUserId,
    ) -> Result<Option<LinkedIdentity>, SlackError>;

    /// Establish the link from a signed-in Relay user to a Slack user
    /// id. Used by the Phase 2 linking flow (issue #41).
    async fn link(
        &self,
        principal: &Principal,
        team_id: &SlackTeamId,
        slack_user_id: &SlackUserId,
    ) -> Result<(), SlackError>;

    /// Tear down the link. RLS-scoped: only the owning org can unlink.
    async fn unlink(
        &self,
        principal: &Principal,
        team_id: &SlackTeamId,
        slack_user_id: &SlackUserId,
    ) -> Result<(), SlackError>;
}

pub type SharedSlackIdentityStore = Arc<dyn SlackIdentityStore>;

pub struct PgSlackIdentityStore {
    pool: PgPool,
    clock: SharedClock,
}

impl PgSlackIdentityStore {
    #[must_use]
    pub fn new(pool: PgPool, clock: SharedClock) -> Self {
        Self { pool, clock }
    }
}

impl fmt::Debug for PgSlackIdentityStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PgSlackIdentityStore")
            .finish_non_exhaustive()
    }
}

#[async_trait]
impl SlackIdentityStore for PgSlackIdentityStore {
    async fn lookup(
        &self,
        team_id: &SlackTeamId,
        slack_user_id: &SlackUserId,
    ) -> Result<Option<LinkedIdentity>, SlackError> {
        let row: Option<(UserId, OrgId)> =
            run_privileged::<Option<(UserId, OrgId)>, SlackError>(&self.pool, async |tx| {
                Ok(sqlx::query_as(
                    "SELECT user_id, org_id FROM slack_identities \
                     WHERE team_id = $1 AND slack_user_id = $2",
                )
                .bind(team_id.as_str())
                .bind(slack_user_id.as_str())
                .fetch_optional(&mut **tx)
                .await?)
            })
            .await?;
        Ok(row.map(|(user_id, org_id)| LinkedIdentity { user_id, org_id }))
    }

    async fn link(
        &self,
        principal: &Principal,
        team_id: &SlackTeamId,
        slack_user_id: &SlackUserId,
    ) -> Result<(), SlackError> {
        let now = self.clock.now_utc();
        let org_id = principal.active_org_id;
        let user_id = principal.user_id;
        let team = team_id.as_str().to_owned();
        let slack_user = slack_user_id.as_str().to_owned();
        run_as_user::<(), SlackError>(&self.pool, user_id, async move |tx| {
            sqlx::query(
                "INSERT INTO slack_identities \
                   (org_id, team_id, slack_user_id, user_id, linked_at) \
                 VALUES ($1, $2, $3, $4, $5) \
                 ON CONFLICT (org_id, team_id, slack_user_id) DO UPDATE SET \
                   user_id = EXCLUDED.user_id, \
                   linked_at = EXCLUDED.linked_at",
            )
            .bind(org_id)
            .bind(&team)
            .bind(&slack_user)
            .bind(user_id)
            .bind(now)
            .execute(&mut **tx)
            .await?;
            Ok(())
        })
        .await
    }

    async fn unlink(
        &self,
        principal: &Principal,
        team_id: &SlackTeamId,
        slack_user_id: &SlackUserId,
    ) -> Result<(), SlackError> {
        let org_id = principal.active_org_id;
        let user_id = principal.user_id;
        let team = team_id.as_str().to_owned();
        let slack_user = slack_user_id.as_str().to_owned();
        run_as_user::<(), SlackError>(&self.pool, user_id, async move |tx| {
            sqlx::query(
                "DELETE FROM slack_identities \
                 WHERE org_id = $1 AND team_id = $2 AND slack_user_id = $3",
            )
            .bind(org_id)
            .bind(&team)
            .bind(&slack_user)
            .execute(&mut **tx)
            .await?;
            Ok(())
        })
        .await
    }
}
