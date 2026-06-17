//! Postgres-backed [`ApprovalStore`] + [`GatedToolStore`] (one struct, two
//! traits — they share the same pool/clock and the same migration).
//!
//! All wall-clock values come from the injected [`SharedClock`] (CLAUDE.md §11);
//! never `NOW()` in app SQL. Tenant-side writes open `run_as_user`; webhook-side
//! reads/decide/sweep open `run_privileged` (no Patom principal — the unguessable
//! id is the capability, `org_id` is the tenancy guard). No `format!` into a
//! query carries a value (CLAUDE.md §10) — the only interpolation is the static
//! `SELECT_COLUMNS` allowlist.

use std::fmt;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sqlx::postgres::PgRow;
use sqlx::{PgConnection, PgPool, Row as _};

use crate::agents::AgentId;
use crate::auth::{Caller, OrgId, run_as_user, run_privileged};
use crate::clock::SharedClock;
use crate::colleagues::ColleagueId;
use crate::runtime::PromptRequestId;
use crate::types::ToolName;

use super::config::GatedToolStore;
use super::error::ApprovalError;
use super::limits::APPROVAL_SWEEP_BATCH;
use super::store::{ApprovalStore, CreateOutcome, DecideOutcome, NewApproval};
use super::types::{
    ActionSummary, ApprovalId, ApprovalRecord, ApprovalStatus, ApproverKind, Decision,
    PlatformMessageId, policy_allows,
};

pub struct PgApprovalStore {
    pool: PgPool,
    clock: SharedClock,
}

impl PgApprovalStore {
    #[must_use]
    pub fn new(pool: PgPool, clock: SharedClock) -> Self {
        Self { pool, clock }
    }
}

impl fmt::Debug for PgApprovalStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PgApprovalStore").finish_non_exhaustive()
    }
}

/// Column projection for every `pending_approval` read. A static allowlist (not
/// a value), so interpolating it into the query string is safe (CLAUDE.md §10).
const SELECT_COLUMNS: &str = "id, org_id, thread_id, requesting_agent_id, \
     requesting_colleague_id, root_request_id, action_summary, gated_tool, approver_kind, \
     approver_colleague, status, platform, platform_app_id, platform_container, \
     platform_reply_to, platform_message_id, decided_by_colleague, decided_at, expires_at, \
     created_at";

/// Parse a `pending_approval` row into the typed read model at the boundary
/// (CLAUDE.md §1). Every `String` column funnels through its newtype's
/// `TryFrom`, so nothing downstream sees an unvalidated value.
fn record_from_row(row: &PgRow) -> Result<ApprovalRecord, ApprovalError> {
    let action_summary = ActionSummary::try_from(row.try_get::<String, _>("action_summary")?)?;
    let gated_tool = ToolName::try_from(row.try_get::<String, _>("gated_tool")?.as_str())?;
    let platform_message_id = row
        .try_get::<Option<String>, _>("platform_message_id")?
        .map(PlatformMessageId::try_from)
        .transpose()?;
    Ok(ApprovalRecord {
        id: row.try_get("id")?,
        org_id: row.try_get("org_id")?,
        thread_id: row.try_get("thread_id")?,
        requesting_agent_id: row.try_get("requesting_agent_id")?,
        requesting_colleague_id: row.try_get("requesting_colleague_id")?,
        root_request_id: row.try_get("root_request_id")?,
        action_summary,
        gated_tool,
        approver_kind: row.try_get("approver_kind")?,
        approver_colleague: row.try_get("approver_colleague")?,
        status: row.try_get("status")?,
        platform: row.try_get("platform")?,
        platform_app_id: row.try_get("platform_app_id")?,
        platform_container: row.try_get("platform_container")?,
        platform_reply_to: row.try_get("platform_reply_to")?,
        platform_message_id,
        decided_by_colleague: row.try_get("decided_by_colleague")?,
        decided_at: row.try_get("decided_at")?,
        expires_at: row.try_get("expires_at")?,
        created_at: row.try_get("created_at")?,
    })
}

/// Insert the row, returning the inserted row or `None` on the idempotency
/// conflict. The static `SELECT_COLUMNS` is the only interpolated text.
async fn insert_row(
    conn: &mut PgConnection,
    new: &NewApproval,
    org_id: OrgId,
    now: DateTime<Utc>,
) -> Result<Option<PgRow>, sqlx::Error> {
    let (app_id, container, reply_to) = new.target.columns();
    let sql = format!(
        "INSERT INTO pending_approval (id, org_id, thread_id, requesting_agent_id, \
             requesting_colleague_id, root_request_id, action_summary, gated_tool, \
             approver_kind, approver_colleague, platform, platform_app_id, platform_container, \
             platform_reply_to, idempotency_key, expires_at, created_at) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17) \
         ON CONFLICT (org_id, idempotency_key) DO NOTHING RETURNING {SELECT_COLUMNS}"
    );
    sqlx::query(&sql)
        .bind(new.id)
        .bind(org_id)
        .bind(new.thread_id)
        .bind(new.requesting_agent_id)
        .bind(new.requesting_colleague_id)
        .bind(new.root_request_id)
        .bind(new.action_summary.as_str())
        .bind(new.gated_tool.as_str())
        .bind(new.approvers.kind().as_str())
        .bind(new.approvers.pinned())
        .bind(new.target.platform().as_str())
        .bind(app_id)
        .bind(container)
        .bind(reply_to)
        .bind(&new.idempotency_key)
        .bind(new.expires_at)
        .bind(now)
        .fetch_optional(conn)
        .await
}

/// Insert the `OneOf` whitelist child rows (bounded by `MAX_APPROVERS` at the
/// tool boundary). No-op for `Anyone`/`Colleague`.
async fn insert_approvers(
    conn: &mut PgConnection,
    new: &NewApproval,
    org_id: OrgId,
) -> Result<(), sqlx::Error> {
    for colleague in new.approvers.members() {
        sqlx::query(
            "INSERT INTO pending_approval_approvers (approval_id, colleague_id, org_id) \
             VALUES ($1, $2, $3) ON CONFLICT DO NOTHING",
        )
        .bind(new.id)
        .bind(*colleague)
        .bind(org_id)
        .execute(&mut *conn)
        .await?;
    }
    Ok(())
}

async fn select_by_idem(
    conn: &mut PgConnection,
    org_id: OrgId,
    key: &str,
) -> Result<Option<PgRow>, sqlx::Error> {
    let sql = format!(
        "SELECT {SELECT_COLUMNS} FROM pending_approval WHERE org_id = $1 AND idempotency_key = $2"
    );
    sqlx::query(&sql)
        .bind(org_id)
        .bind(key)
        .fetch_optional(conn)
        .await
}

async fn select_approver_ids(
    conn: &mut PgConnection,
    id: ApprovalId,
) -> Result<Vec<ColleagueId>, sqlx::Error> {
    let rows: Vec<(ColleagueId,)> = sqlx::query_as(
        "SELECT colleague_id FROM pending_approval_approvers WHERE approval_id = $1",
    )
    .bind(id)
    .fetch_all(conn)
    .await?;
    Ok(rows.into_iter().map(|(c,)| c).collect())
}

#[async_trait]
impl ApprovalStore for PgApprovalStore {
    async fn create(
        &self,
        caller: &Caller,
        new: NewApproval,
    ) -> Result<CreateOutcome, ApprovalError> {
        let org_id = caller.org_id;
        let now = self.clock.now_utc();
        run_as_user(&self.pool, caller.user_id, async move |tx| {
            if let Some(row) = insert_row(tx, &new, org_id, now).await? {
                insert_approvers(tx, &new, org_id).await?;
                return Ok(CreateOutcome::Created(record_from_row(&row)?));
            }
            let row = select_by_idem(tx, org_id, &new.idempotency_key)
                .await?
                .ok_or(ApprovalError::NotFound)?;
            Ok(CreateOutcome::Existing(record_from_row(&row)?))
        })
        .await
    }

    async fn attach_message(
        &self,
        caller: &Caller,
        id: ApprovalId,
        message_id: PlatformMessageId,
    ) -> Result<(), ApprovalError> {
        let org_id = caller.org_id;
        run_as_user(&self.pool, caller.user_id, async move |tx| {
            let affected = sqlx::query(
                "UPDATE pending_approval SET platform_message_id = $1 WHERE id = $2 AND org_id = $3",
            )
            .bind(message_id.as_str())
            .bind(id)
            .bind(org_id)
            .execute(&mut **tx)
            .await?
            .rows_affected();
            if affected == 0 {
                return Err(ApprovalError::NotFound);
            }
            Ok(())
        })
        .await
    }

    async fn read(&self, org_id: OrgId, id: ApprovalId) -> Result<ApprovalRecord, ApprovalError> {
        run_privileged(&self.pool, async move |tx| {
            let sql = format!(
                "SELECT {SELECT_COLUMNS} FROM pending_approval WHERE id = $1 AND org_id = $2"
            );
            let row = sqlx::query(&sql)
                .bind(id)
                .bind(org_id)
                .fetch_optional(&mut **tx)
                .await?
                .ok_or(ApprovalError::NotFound)?;
            record_from_row(&row)
        })
        .await
    }

    async fn decide(
        &self,
        org_id: OrgId,
        id: ApprovalId,
        decision: Decision,
        clicker: ColleagueId,
        now: DateTime<Utc>,
    ) -> Result<DecideOutcome, ApprovalError> {
        run_privileged(&self.pool, async move |tx| {
            // Serialize concurrent clicks: FOR UPDATE makes the second click
            // wait, then observe the now-resolved status (idempotent double-click).
            let lock_sql =
                format!("SELECT {SELECT_COLUMNS} FROM pending_approval WHERE id = $1 AND org_id = $2 FOR UPDATE");
            let row = sqlx::query(&lock_sql)
                .bind(id)
                .bind(org_id)
                .fetch_optional(&mut **tx)
                .await?
                .ok_or(ApprovalError::NotFound)?;
            let record = record_from_row(&row)?;
            match record.status {
                ApprovalStatus::Approved | ApprovalStatus::Denied => {
                    return Ok(DecideOutcome::AlreadyDecided(record));
                }
                ApprovalStatus::Expired => return Err(ApprovalError::Expired),
                ApprovalStatus::Pending => {}
            }
            if record.expires_at <= now {
                return Err(ApprovalError::Expired);
            }
            // Server-side authorization before the flip (CLAUDE.md §Authorization).
            let one_of = if record.approver_kind == ApproverKind::OneOf {
                select_approver_ids(tx, id).await?
            } else {
                Vec::new()
            };
            if !policy_allows(record.approver_kind, record.approver_colleague, &one_of, clicker) {
                return Err(ApprovalError::Unauthorized);
            }
            let update_sql = format!(
                "UPDATE pending_approval SET status = $1, decided_by_colleague = $2, \
                 decided_at = $3 WHERE id = $4 AND org_id = $5 AND status = 'pending' \
                 RETURNING {SELECT_COLUMNS}"
            );
            let updated = sqlx::query(&update_sql)
                .bind(decision.status())
                .bind(clicker)
                .bind(now)
                .bind(id)
                .bind(org_id)
                .fetch_one(&mut **tx)
                .await?;
            Ok(DecideOutcome::Decided(record_from_row(&updated)?))
        })
        .await
    }

    async fn expire_due(&self, now: DateTime<Utc>) -> Result<u64, ApprovalError> {
        run_privileged(&self.pool, async move |tx| {
            let affected = sqlx::query(
                "UPDATE pending_approval SET status = 'expired' WHERE id IN (\
                     SELECT id FROM pending_approval \
                      WHERE status = 'pending' AND expires_at <= $1 \
                      ORDER BY expires_at LIMIT $2 FOR UPDATE SKIP LOCKED)",
            )
            .bind(now)
            .bind(APPROVAL_SWEEP_BATCH)
            .execute(&mut **tx)
            .await?
            .rows_affected();
            Ok(affected)
        })
        .await
    }

    async fn has_approved_for_dag(
        &self,
        org_id: OrgId,
        root: PromptRequestId,
        tool: &ToolName,
    ) -> Result<bool, ApprovalError> {
        let tool = tool.as_str().to_owned();
        run_privileged(&self.pool, async move |tx| {
            let (exists,): (bool,) = sqlx::query_as(
                "SELECT EXISTS(SELECT 1 FROM pending_approval \
                     WHERE root_request_id = $1 AND org_id = $2 AND status = 'approved' \
                       AND gated_tool = $3)",
            )
            .bind(root)
            .bind(org_id)
            .bind(&tool)
            .fetch_one(&mut **tx)
            .await?;
            Ok(exists)
        })
        .await
    }
}

#[async_trait]
impl GatedToolStore for PgApprovalStore {
    async fn is_gated(
        &self,
        org_id: OrgId,
        agent_id: AgentId,
        tool: &ToolName,
    ) -> Result<bool, ApprovalError> {
        let tool = tool.as_str().to_owned();
        run_privileged(&self.pool, async move |tx| {
            let (exists,): (bool,) = sqlx::query_as(
                "SELECT EXISTS(SELECT 1 FROM agent_gated_tools \
                     WHERE agent_id = $1 AND org_id = $2 AND tool_name = $3)",
            )
            .bind(agent_id)
            .bind(org_id)
            .bind(&tool)
            .fetch_one(&mut **tx)
            .await?;
            Ok(exists)
        })
        .await
    }

    async fn gated_tools_for_agent(
        &self,
        org_id: OrgId,
        agent_id: AgentId,
    ) -> Result<Vec<ToolName>, ApprovalError> {
        let rows: Vec<(String,)> =
            run_privileged::<Vec<(String,)>, ApprovalError>(&self.pool, async move |tx| {
                Ok(sqlx::query_as(
                    "SELECT tool_name FROM agent_gated_tools \
                     WHERE agent_id = $1 AND org_id = $2 ORDER BY tool_name",
                )
                .bind(agent_id)
                .bind(org_id)
                .fetch_all(&mut **tx)
                .await?)
            })
            .await?;
        rows.into_iter()
            .map(|(t,)| ToolName::try_from(t.as_str()).map_err(ApprovalError::from))
            .collect()
    }

    async fn set_gated(
        &self,
        caller: &Caller,
        agent_id: AgentId,
        tool: &ToolName,
    ) -> Result<(), ApprovalError> {
        let org_id = caller.org_id;
        let now = self.clock.now_utc();
        let tool = tool.as_str().to_owned();
        run_as_user(&self.pool, caller.user_id, async move |tx| {
            sqlx::query(
                "INSERT INTO agent_gated_tools (agent_id, tool_name, org_id, created_at) \
                 VALUES ($1, $2, $3, $4) ON CONFLICT (agent_id, tool_name) DO NOTHING",
            )
            .bind(agent_id)
            .bind(&tool)
            .bind(org_id)
            .bind(now)
            .execute(&mut **tx)
            .await?;
            Ok(())
        })
        .await
    }

    async fn unset_gated(
        &self,
        caller: &Caller,
        agent_id: AgentId,
        tool: &ToolName,
    ) -> Result<(), ApprovalError> {
        let org_id = caller.org_id;
        let tool = tool.as_str().to_owned();
        run_as_user(&self.pool, caller.user_id, async move |tx| {
            sqlx::query(
                "DELETE FROM agent_gated_tools WHERE agent_id = $1 AND tool_name = $2 AND org_id = $3",
            )
            .bind(agent_id)
            .bind(&tool)
            .bind(org_id)
            .execute(&mut **tx)
            .await?;
            Ok(())
        })
        .await
    }
}
