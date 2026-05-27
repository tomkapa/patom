//! Postgres-backed [`ScheduledTaskStore`].
//!
//! All wall-clock values come from the injected [`SharedClock`] — never
//! `NOW()` in app SQL — so a `TestClock`-driven test sees stable
//! `created_at` / `updated_at` (CLAUDE.md §11). The `schedule` JSONB
//! column is serialised via serde at the boundary; nothing downstream
//! sees a raw `serde_json::Value`.

use std::fmt;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sqlx::PgPool;

use crate::agents::AgentId;
use crate::auth::{OrgId, UserId, run_as_user, run_privileged};
use crate::clock::SharedClock;
use crate::runtime::PromptRequestId;

use super::error::ScheduledTaskError;
use super::limits::MAX_SCHEDULED_TASKS_PER_AGENT;
use super::store::{NewScheduledTask, ScheduledTaskStore};
use super::types::{
    ScheduledPrompt, ScheduledTaskId, ScheduledTaskName, ScheduledTaskRecord, ScheduledTaskState,
};

pub struct PgScheduledTaskStore {
    pool: PgPool,
    clock: SharedClock,
}

impl PgScheduledTaskStore {
    #[must_use]
    pub fn new(pool: PgPool, clock: SharedClock) -> Self {
        Self { pool, clock }
    }

    fn now(&self) -> DateTime<Utc> {
        self.clock.now_utc()
    }
}

impl fmt::Debug for PgScheduledTaskStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PgScheduledTaskStore")
            .finish_non_exhaustive()
    }
}

/// SELECT projection used by every read path. Order must match
/// [`row_to_record`]. `org_id` + `created_by_user_id` are the tenancy
/// columns added by migration 19; they round-trip on every read so the
/// scheduler can enqueue without a follow-up JOIN.
const SELECT_COLUMNS: &str = "id, owner_agent_id, org_id, created_by_user_id, \
     name, prompt, schedule, \
     next_run_at, last_fired_at, last_request_id, state, created_at, updated_at";

#[allow(clippy::type_complexity)]
type Row = (
    ScheduledTaskId,
    AgentId,
    OrgId,
    UserId,
    String,
    String,
    serde_json::Value,
    Option<DateTime<Utc>>,
    Option<DateTime<Utc>>,
    Option<PromptRequestId>,
    ScheduledTaskState,
    DateTime<Utc>,
    DateTime<Utc>,
);

fn row_to_record(row: Row) -> Result<ScheduledTaskRecord, ScheduledTaskError> {
    let (
        id,
        owner_agent_id,
        org_id,
        created_by_user_id,
        name,
        prompt,
        schedule,
        next_run_at,
        last_fired_at,
        last_request_id,
        state,
        created_at,
        updated_at,
    ) = row;
    Ok(ScheduledTaskRecord {
        id,
        owner_agent_id,
        org_id,
        created_by_user_id,
        name: ScheduledTaskName::try_from(name)?,
        prompt: ScheduledPrompt::try_from(prompt)?,
        schedule: serde_json::from_value(schedule)?,
        next_run_at,
        last_fired_at,
        last_request_id,
        state,
        created_at,
        updated_at,
    })
}

#[async_trait]
impl ScheduledTaskStore for PgScheduledTaskStore {
    async fn create(
        &self,
        payload: NewScheduledTask,
    ) -> Result<ScheduledTaskRecord, ScheduledTaskError> {
        run_privileged(&self.pool, async |tx| {
            create_in_tx(self, tx.tx_mut(), payload).await
        })
        .await
    }

    async fn create_for_user(
        &self,
        acting_user_id: UserId,
        mut payload: NewScheduledTask,
    ) -> Result<ScheduledTaskRecord, ScheduledTaskError> {
        // Identity invariant: persist the authenticated actor regardless
        // of the payload's `created_by_user_id`. Otherwise a caller
        // could schedule a task that fires under another member's
        // principal (the scheduler reads this column to mint the
        // resulting `NewPromptRequest`'s tenancy).
        payload.created_by_user_id = acting_user_id;
        run_as_user(&self.pool, acting_user_id, async |tx| {
            create_in_tx(self, tx.tx_mut(), payload).await
        })
        .await
    }

    async fn cancel_for_user(
        &self,
        acting_user_id: UserId,
        task: ScheduledTaskId,
        owner: AgentId,
    ) -> Result<(), ScheduledTaskError> {
        run_as_user(&self.pool, acting_user_id, async |tx| {
            cancel_in_tx(self, tx.tx_mut(), task, owner).await
        })
        .await
    }

    async fn list_for_agent(
        &self,
        owner: AgentId,
    ) -> Result<Vec<ScheduledTaskRecord>, ScheduledTaskError> {
        // §5: explicit upper bound. The create path enforces
        // MAX_SCHEDULED_TASKS_PER_AGENT, so the LIMIT is a defence in
        // depth against drift in that invariant.
        let cap = i64::try_from(MAX_SCHEDULED_TASKS_PER_AGENT).map_err(|_| {
            ScheduledTaskError::Db(sqlx::Error::Decode(
                format!("invariant: cap {MAX_SCHEDULED_TASKS_PER_AGENT} exceeds i64").into(),
            ))
        })?;
        let rows = run_privileged::<Vec<Row>, ScheduledTaskError>(&self.pool, async |tx| {
            Ok(sqlx::query_as(&format!(
                "SELECT {SELECT_COLUMNS} FROM scheduled_tasks \
                 WHERE owner_agent_id = $1 AND state = $2 \
                 ORDER BY created_at ASC \
                 LIMIT $3"
            ))
            .bind(owner)
            .bind(ScheduledTaskState::Active)
            .bind(cap)
            .fetch_all(&mut **tx)
            .await?)
        })
        .await?;
        assert!(
            rows.len() <= MAX_SCHEDULED_TASKS_PER_AGENT,
            "invariant: list_for_agent exceeded per-agent cap"
        );

        rows.into_iter().map(row_to_record).collect()
    }

    async fn cancel(
        &self,
        task: ScheduledTaskId,
        owner: AgentId,
    ) -> Result<(), ScheduledTaskError> {
        run_privileged(&self.pool, async |tx| {
            cancel_in_tx(self, tx.tx_mut(), task, owner).await
        })
        .await
    }

    async fn claim_due(
        &self,
        now: DateTime<Utc>,
        limit: usize,
    ) -> Result<Vec<ScheduledTaskRecord>, ScheduledTaskError> {
        // Plain SELECT — no row-level lock. Concurrent scheduler nodes
        // (future) dedupe at the queue layer via the
        // `sched-{task_id}-{fire_ts}` idempotency key; `record_fired`
        // is idempotent over the same `(fired_at, next_run_at)` pair.
        //
        // Cross-tenant scan: the scheduler ticks across every org, so
        // the privileged tx bypasses the RLS policy installed in
        // migration 19. Each returned row carries its own `org_id` +
        // `created_by_user_id` so the caller pins the enqueued
        // `NewPromptRequest` to the right tenant per row.
        let limit_i64 = i64::try_from(limit).map_err(|_| {
            ScheduledTaskError::Db(sqlx::Error::Decode(
                format!("invariant: limit {limit} exceeds i64").into(),
            ))
        })?;
        let rows = run_privileged::<Vec<Row>, ScheduledTaskError>(&self.pool, async |tx| {
            Ok(sqlx::query_as(&format!(
                "SELECT {SELECT_COLUMNS} FROM scheduled_tasks \
                 WHERE state = $1 \
                   AND next_run_at IS NOT NULL \
                   AND next_run_at <= $2 \
                 ORDER BY next_run_at ASC \
                 LIMIT $3"
            ))
            .bind(ScheduledTaskState::Active)
            .bind(now)
            .bind(limit_i64)
            .fetch_all(&mut **tx)
            .await?)
        })
        .await?;

        rows.into_iter().map(row_to_record).collect()
    }

    async fn record_fired(
        &self,
        task: ScheduledTaskId,
        request_id: PromptRequestId,
        fired_at: DateTime<Utc>,
        next_run_at: Option<DateTime<Utc>>,
    ) -> Result<(), ScheduledTaskError> {
        let now = self.now();
        // `next_run_at IS NULL` ⇒ schedule exhausted ⇒ flip state to Done.
        let next_state = if next_run_at.is_some() {
            ScheduledTaskState::Active
        } else {
            ScheduledTaskState::Done
        };
        run_privileged::<(), ScheduledTaskError>(&self.pool, async |tx| {
            sqlx::query(
                "UPDATE scheduled_tasks \
                 SET last_fired_at = $1, \
                     last_request_id = $2, \
                     next_run_at = $3, \
                     state = $4, \
                     updated_at = $5 \
                 WHERE id = $6",
            )
            .bind(fired_at)
            .bind(request_id)
            .bind(next_run_at)
            .bind(next_state)
            .bind(now)
            .bind(task)
            .execute(&mut **tx)
            .await?;
            Ok(())
        })
        .await
    }
}

/// Body of `create` / `create_for_user`. The runner owns commit/rollback,
/// so this helper only runs SQL. The opened tx is either privileged
/// (scheduler / HTTP) or `run_as_user` (the `schedule_task` tool, so the
/// INSERT runs RLS-checked) — the choice lives in the caller's runner
/// call.
async fn create_in_tx(
    store: &PgScheduledTaskStore,
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    payload: NewScheduledTask,
) -> Result<ScheduledTaskRecord, ScheduledTaskError> {
    let id = ScheduledTaskId::new();
    let now = store.now();
    let schedule_json = serde_json::to_value(&payload.schedule)?;
    let owner = payload.owner_agent_id;
    let cap_label = format!("patom:sched_cap:{owner}");

    // Per-agent advisory lock — pg_advisory_xact_lock serializes
    // concurrent create-for-same-owner transactions so the count +
    // insert below cannot race past the cap (READ COMMITTED alone
    // does not prevent the TOCTOU).
    sqlx::query("SELECT pg_advisory_xact_lock(hashtext($1))")
        .bind(&cap_label)
        .execute(&mut **tx)
        .await?;

    let (count,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM scheduled_tasks \
             WHERE owner_agent_id = $1 AND state = $2",
    )
    .bind(owner)
    .bind(ScheduledTaskState::Active)
    .fetch_one(&mut **tx)
    .await?;
    let count_usize = usize::try_from(count).map_err(|_| {
        ScheduledTaskError::Db(sqlx::Error::Decode(
            format!("invariant: negative count {count}").into(),
        ))
    })?;
    if count_usize >= MAX_SCHEDULED_TASKS_PER_AGENT {
        return Err(ScheduledTaskError::PerAgentCapExceeded {
            agent: owner,
            max: MAX_SCHEDULED_TASKS_PER_AGENT,
        });
    }

    let row: Row = sqlx::query_as(&format!(
        "INSERT INTO scheduled_tasks \
                ({SELECT_COLUMNS}) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, NULL, NULL, $9, $10, $10) \
             RETURNING {SELECT_COLUMNS}"
    ))
    .bind(id)
    .bind(owner)
    .bind(payload.org_id)
    .bind(payload.created_by_user_id)
    .bind(payload.name.as_str())
    .bind(payload.prompt.as_str())
    .bind(&schedule_json)
    .bind(payload.next_run_at)
    .bind(ScheduledTaskState::Active)
    .bind(now)
    .fetch_one(&mut **tx)
    .await?;

    row_to_record(row)
}

/// Body of `cancel` / `cancel_for_user`.
async fn cancel_in_tx(
    store: &PgScheduledTaskStore,
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    task: ScheduledTaskId,
    owner: AgentId,
) -> Result<(), ScheduledTaskError> {
    let now = store.now();

    let existing: Option<(ScheduledTaskState,)> = sqlx::query_as(
        "SELECT state FROM scheduled_tasks \
             WHERE id = $1 AND owner_agent_id = $2 FOR UPDATE",
    )
    .bind(task)
    .bind(owner)
    .fetch_optional(&mut **tx)
    .await?;

    let Some((state,)) = existing else {
        return Err(ScheduledTaskError::NotFound(task));
    };
    if !matches!(state, ScheduledTaskState::Active) {
        return Ok(());
    }

    sqlx::query(
        "UPDATE scheduled_tasks \
             SET state = $1, next_run_at = NULL, updated_at = $2 \
             WHERE id = $3",
    )
    .bind(ScheduledTaskState::Cancelled)
    .bind(now)
    .bind(task)
    .execute(&mut **tx)
    .await?;

    Ok(())
}
