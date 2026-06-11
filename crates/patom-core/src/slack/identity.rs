//! Slack-user ↔ Patom-user link — `slack_identities` table.
//!
//! A Slack user is bound to the Patom account they authenticate as: the
//! `/patom` slash command gates on a linked identity, and an unlinked
//! user is sent through the normal Patom login (issue #41, "Alt A"). We
//! never provision a Patom account from Slack data — the account is born
//! from the IdP, and [`SlackIdentityStore::link_with_org`] merely binds
//! the `slack_user_id` to it in the workspace's org. The workspace
//! installer is auto-linked at OAuth install ([`LinkedVia::Installer`]).
//!
//! There is no Phase-1 installer fallback: an unlinked Slack user resolves
//! to no Patom identity (the bridge nudges them to `/patom`).

use std::fmt;
use std::sync::Arc;

use async_trait::async_trait;
use sqlx::PgPool;

use crate::auth::{OrgId, Principal, UserId, run_as_user, run_privileged};
use crate::clock::SharedClock;

use super::error::SlackError;
use super::types::{SlackTeamId, SlackUserId};

/// Resolved Patom-side identity for an inbound Slack event.
#[derive(Debug, Clone, Copy)]
pub struct LinkedIdentity {
    pub user_id: UserId,
    pub org_id: OrgId,
}

/// Provenance of a `slack_identities` row.
///
/// Mirrors the `linked_via` CHECK constraint (migration 72). A domain
/// enum mapped to a `&'static str` keeps the column value out of
/// caller-controlled strings (CLAUDE.md §10) and exhaustive at every
/// write site (§1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkedVia {
    /// Written at OAuth install from the Slack `authed_user.id`.
    Installer,
    /// Written by the post-login completion route after `/patom`.
    SlackOauth,
}

impl LinkedVia {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Installer => "installer",
            Self::SlackOauth => "slack_oauth",
        }
    }
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

    /// Bind a Slack user to a Patom account in an explicit org, ensuring
    /// the user is a member of that org first (so the colleague trigger
    /// mints their Human colleague). Used by the post-login completion
    /// route — whose `Principal.active_org_id` is the wrong or absent org
    /// for a freshly-onboarded user — and by installer auto-link. Writes
    /// `linked_via` provenance and is idempotent (re-link updates the
    /// `user_id`). Privileged: the org is the workspace's, derived from a
    /// verified signed token, not from a session.
    async fn link_with_org(
        &self,
        user_id: UserId,
        org_id: OrgId,
        team_id: &SlackTeamId,
        slack_user_id: &SlackUserId,
        linked_via: LinkedVia,
    ) -> Result<(), SlackError>;

    /// Record the Slack workspace display name for a linked user. This is
    /// a per-platform display *label* — the agent renders it when talking
    /// in a Slack thread so it uses the name the user's teammates know in
    /// Slack — and is keyed/joined by `colleague_id` elsewhere; it never
    /// touches `users.display_name` (Patom identity is not derived from
    /// Slack). Privileged; a no-op when `display_name` is empty or no
    /// `slack_identities` row exists yet for `(team, slack_user)`.
    async fn set_display_name(
        &self,
        team_id: &SlackTeamId,
        slack_user_id: &SlackUserId,
        display_name: &str,
    ) -> Result<(), SlackError>;

    /// Tear down the link. RLS-scoped by org membership: the caller may
    /// unlink any identity in a workspace whose org they belong to. The
    /// row is matched globally by `(team_id, slack_user_id)` (a UNIQUE
    /// index) and filtered by the `app_user_is_member(org_id)` policy, so
    /// the caller's *active* org is irrelevant.
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

    async fn link_with_org(
        &self,
        user_id: UserId,
        org_id: OrgId,
        team_id: &SlackTeamId,
        slack_user_id: &SlackUserId,
        linked_via: LinkedVia,
    ) -> Result<(), SlackError> {
        let now = self.clock.now_utc();
        let team = team_id.as_str().to_owned();
        let slack_user = slack_user_id.as_str().to_owned();
        let via = linked_via.as_str();
        // Privileged: the org is the workspace's (from a verified token),
        // and the membership insert below establishes the very RLS
        // membership a tenant-scoped tx would require — so the two writes
        // happen together, RLS-free, in one atomic transaction.
        run_privileged::<(), SlackError>(&self.pool, async move |tx| {
            // Ensure org membership first — the `org_members_mint_colleague`
            // trigger (migration 58) creates the Human colleague the agent
            // addresses. DO NOTHING preserves an existing role (e.g. the
            // installer stays 'owner').
            sqlx::query(
                "INSERT INTO org_members (org_id, user_id, role, created_at) \
                 VALUES ($1, $2, 'member', $3) \
                 ON CONFLICT (org_id, user_id) DO NOTHING",
            )
            .bind(org_id)
            .bind(user_id)
            .bind(now)
            .execute(&mut **tx)
            .await?;

            sqlx::query(
                "INSERT INTO slack_identities \
                   (org_id, team_id, slack_user_id, user_id, linked_at, linked_via) \
                 VALUES ($1, $2, $3, $4, $5, $6) \
                 ON CONFLICT (org_id, team_id, slack_user_id) DO UPDATE SET \
                   user_id = EXCLUDED.user_id, \
                   linked_at = EXCLUDED.linked_at, \
                   linked_via = EXCLUDED.linked_via",
            )
            .bind(org_id)
            .bind(&team)
            .bind(&slack_user)
            .bind(user_id)
            .bind(now)
            .bind(via)
            .execute(&mut **tx)
            .await?;
            Ok(())
        })
        .await
    }

    async fn set_display_name(
        &self,
        team_id: &SlackTeamId,
        slack_user_id: &SlackUserId,
        display_name: &str,
    ) -> Result<(), SlackError> {
        if display_name.is_empty() {
            return Ok(());
        }
        let team = team_id.as_str().to_owned();
        let slack_user = slack_user_id.as_str().to_owned();
        let name = display_name.to_owned();
        // Privileged: keyed by (team, slack_user) from a verified Slack
        // context, no Principal. Only updates the Slack-side label column.
        run_privileged::<(), SlackError>(&self.pool, async move |tx| {
            sqlx::query(
                "UPDATE slack_identities SET display_name = $3 \
                 WHERE team_id = $1 AND slack_user_id = $2",
            )
            .bind(&team)
            .bind(&slack_user)
            .bind(&name)
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
        let user_id = principal.user_id;
        let team = team_id.as_str().to_owned();
        let slack_user = slack_user_id.as_str().to_owned();
        // No org_id predicate: the `(team_id, slack_user_id)` UNIQUE index
        // pins at most one row, and the `app_user_is_member(org_id)` RLS
        // policy filters it to orgs the caller belongs to — correct even
        // when the caller's active org differs from the workspace's org.
        run_as_user::<(), SlackError>(&self.pool, user_id, async move |tx| {
            sqlx::query(
                "DELETE FROM slack_identities \
                 WHERE team_id = $1 AND slack_user_id = $2",
            )
            .bind(&team)
            .bind(&slack_user)
            .execute(&mut **tx)
            .await?;
            Ok(())
        })
        .await
    }
}
