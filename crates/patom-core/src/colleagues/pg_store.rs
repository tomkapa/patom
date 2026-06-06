//! Postgres-backed [`ColleagueStore`].
//!
//! Reads run privileged (RLS bypassed) for two reasons: the roster join reads
//! `users`, which is REVOKEd from the `patom_app` role, and the read is already
//! org-scoped by the bound `org_id` parameter — mirroring [`crate::agents`]'s
//! `list_*` shape. The `colleagues` RLS policy remains as tenant-isolation
//! defence for any direct `patom_app` query.

use async_trait::async_trait;
use sqlx::PgPool;

use crate::agents::AgentId;
use crate::auth::{OrgId, UserId, run_privileged};

use super::error::ColleagueError;
use super::limits::COLLEAGUE_ROSTER_FETCH_MAX;
use super::store::ColleagueStore;
use super::types::{Colleague, ColleagueId, ColleagueKind, ColleagueName, ColleagueRef};

/// Resolve a colleague's display name from whichever satellite backs it. Agents
/// carry `agents.name`; humans carry `users.display_name`, falling back to the
/// email local-part when the profile name is unset.
///
/// Inlined into the SQL constants below via `concat!` so the queries stay
/// `&'static str` — no runtime `format!` allocation per call.
macro_rules! display_name_expr {
    () => {
        "COALESCE(a.name, u.display_name, split_part(u.email, '@', 1))"
    };
}

/// Roster scan: org-keyed, alpha-sorted by resolved display name, capped.
const LIST_FOR_ORG_SQL: &str = concat!(
    "SELECT c.id, c.kind, ",
    display_name_expr!(),
    " AS display_name \
       FROM colleagues c \
       LEFT JOIN agents a ON a.id = c.agent_id \
       LEFT JOIN users  u ON u.id = c.user_id \
      WHERE c.org_id = $1 \
      ORDER BY lower(",
    display_name_expr!(),
    ") ASC \
      LIMIT $2",
);

/// Point read of a single colleague row, with the joined display name.
const READ_SQL: &str = concat!(
    "SELECT c.id, c.org_id, c.kind, c.user_id, c.agent_id, ",
    display_name_expr!(),
    " AS display_name \
       FROM colleagues c \
       LEFT JOIN agents a ON a.id = c.agent_id \
       LEFT JOIN users  u ON u.id = c.user_id \
      WHERE c.id = $1",
);

/// Postgres-backed directory reader. Cheap clone of a [`PgPool`].
#[derive(Debug, Clone)]
pub struct PgColleagueStore {
    pool: PgPool,
}

impl PgColleagueStore {
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl ColleagueStore for PgColleagueStore {
    async fn list_for_org(&self, org_id: OrgId) -> Result<Vec<ColleagueRef>, ColleagueError> {
        // §10: every value is a bound parameter; the SQL is a compile-time
        // `&'static str` built from `concat!`.
        let rows = run_privileged::<Vec<(ColleagueId, ColleagueKind, String)>, ColleagueError>(
            &self.pool,
            async |tx| {
                Ok(sqlx::query_as(LIST_FOR_ORG_SQL)
                    .bind(org_id)
                    .bind(COLLEAGUE_ROSTER_FETCH_MAX)
                    .fetch_all(&mut **tx)
                    .await?)
            },
        )
        .await?;

        let mut out = Vec::with_capacity(rows.len());
        for (id, kind, display_name) in rows {
            out.push(ColleagueRef {
                id,
                kind,
                display_name: ColleagueName::try_from(display_name)?,
            });
        }
        Ok(out)
    }

    async fn read(&self, id: ColleagueId) -> Result<Colleague, ColleagueError> {
        let row = run_privileged::<
            Option<(
                ColleagueId,
                OrgId,
                ColleagueKind,
                Option<UserId>,
                Option<AgentId>,
                String,
            )>,
            ColleagueError,
        >(&self.pool, async |tx| {
            Ok(sqlx::query_as(READ_SQL)
                .bind(id)
                .fetch_optional(&mut **tx)
                .await?)
        })
        .await?;

        let (cid, org_id, kind, user_id, agent_id, display_name) =
            row.ok_or(ColleagueError::NotFound(id))?;
        Colleague::try_new(
            cid,
            org_id,
            kind,
            ColleagueName::try_from(display_name)?,
            user_id,
            agent_id,
        )
    }

    async fn resolve_agent(
        &self,
        org_id: OrgId,
        agent_id: AgentId,
    ) -> Result<ColleagueId, ColleagueError> {
        resolve_agent_colleague(&self.pool, org_id, agent_id).await
    }

    async fn resolve_user(
        &self,
        org_id: OrgId,
        user_id: UserId,
    ) -> Result<ColleagueId, ColleagueError> {
        resolve_user_colleague(&self.pool, org_id, user_id).await
    }
}

/// Free-function variant of [`ColleagueStore::resolve_agent`].
///
/// Usable from any caller that holds a [`PgPool`] without going through the
/// trait — memory librarian / reflection scheduler / scheduling::scheduler use
/// this so they don't need a `SharedColleagueStore` field for one lookup per
/// tick.
pub async fn resolve_agent_colleague(
    pool: &sqlx::PgPool,
    org_id: OrgId,
    agent_id: AgentId,
) -> Result<ColleagueId, ColleagueError> {
    let row = run_privileged::<Option<(ColleagueId,)>, ColleagueError>(pool, async |tx| {
        Ok(sqlx::query_as(
            "SELECT id FROM colleagues WHERE org_id = $1 AND agent_id = $2",
        )
        .bind(org_id)
        .bind(agent_id)
        .fetch_optional(&mut **tx)
        .await?)
    })
    .await?;
    row.map(|(id,)| id).ok_or(ColleagueError::SatelliteUnmapped {
        kind: ColleagueKind::Agent,
    })
}

/// Free-function variant of [`ColleagueStore::resolve_user`].
pub async fn resolve_user_colleague(
    pool: &sqlx::PgPool,
    org_id: OrgId,
    user_id: UserId,
) -> Result<ColleagueId, ColleagueError> {
    let row = run_privileged::<Option<(ColleagueId,)>, ColleagueError>(pool, async |tx| {
        Ok(sqlx::query_as(
            "SELECT id FROM colleagues WHERE org_id = $1 AND user_id = $2",
        )
        .bind(org_id)
        .bind(user_id)
        .fetch_optional(&mut **tx)
        .await?)
    })
    .await?;
    row.map(|(id,)| id).ok_or(ColleagueError::SatelliteUnmapped {
        kind: ColleagueKind::Human,
    })
}
