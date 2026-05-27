//! `list_scheduled_tasks` — list the caller's active scheduled tasks.

use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::{Value, json};
use tracing::warn;

use sqlx::PgPool;

use crate::auth::begin_as_user;
use crate::scheduling::{
    ScheduleSpec, ScheduledTaskId, ScheduledTaskName, ScheduledTaskRecord, SharedScheduledTaskStore,
};
use crate::session::SharedSessionStore;
use crate::tools::{Tool, ToolCallContext, ToolError};
use crate::types::ToolName;

const TOOL_NAME: &str = "list_scheduled_tasks";
const TOOL_DESCRIPTION: &str = "List your active scheduled tasks. Use this to recall \
    what's already scheduled before creating a new task or to answer the user's question \
    about what they have set up. Returns each task's id, name, schedule, and the next time \
    it will fire. Cancelled or completed tasks are not returned.";

#[derive(Debug, Serialize)]
struct Output {
    tasks: Vec<ListedTask>,
}

#[derive(Debug, Serialize)]
struct ListedTask {
    task_id: ScheduledTaskId,
    name: ScheduledTaskName,
    schedule: ScheduleSpec,
    next_run_at: Option<DateTime<Utc>>,
}

impl From<ScheduledTaskRecord> for ListedTask {
    fn from(r: ScheduledTaskRecord) -> Self {
        Self {
            task_id: r.id,
            name: r.name,
            schedule: r.schedule,
            next_run_at: r.next_run_at,
        }
    }
}

pub struct ListScheduledTasksTool {
    name: ToolName,
    description: &'static str,
    input_schema: Arc<Value>,
    store: SharedScheduledTaskStore,
    sessions: SharedSessionStore,
    pool: PgPool,
}

impl std::fmt::Debug for ListScheduledTasksTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ListScheduledTasksTool")
            .finish_non_exhaustive()
    }
}

impl ListScheduledTasksTool {
    #[must_use]
    pub fn new(
        store: SharedScheduledTaskStore,
        sessions: SharedSessionStore,
        pool: PgPool,
    ) -> Self {
        let name =
            ToolName::try_from(TOOL_NAME).expect("invariant: list_scheduled_tasks valid name");
        let input_schema = Arc::new(json!({
            "type": "object",
            "properties": {},
            "additionalProperties": false
        }));
        Self {
            name,
            description: TOOL_DESCRIPTION,
            input_schema,
            store,
            sessions,
            pool,
        }
    }
}

#[async_trait]
impl Tool for ListScheduledTasksTool {
    fn name(&self) -> &ToolName {
        &self.name
    }
    fn description(&self) -> &str {
        self.description
    }
    fn input_schema(&self) -> Arc<Value> {
        self.input_schema.clone()
    }

    fn concurrency_safe(&self) -> bool {
        true
    }

    #[tracing::instrument(
        skip_all,
        name = "tool.list_scheduled_tasks",
        fields(patom.from.viewer = %ctx.viewer),
    )]
    async fn execute(&self, _input: Value, ctx: &ToolCallContext) -> Result<String, ToolError> {
        let owner = ctx.viewer.agent_id().ok_or_else(|| {
            ToolError::InvalidInput("list_scheduled_tasks: caller must be an agent".into())
        })?;

        // Tenant-side gate mirrors `cancel_scheduled_task`: confirm the
        // owner agent itself is visible under the session principal's
        // RLS view before delegating to the privileged-tx store, so a
        // misrouted call against a cross-org agent returns an empty list
        // rather than that agent's actual schedule.
        let tenancy = self.sessions.tenancy(ctx.session_id).await.map_err(|e| {
            warn!(error = %e, patom.session.id = %ctx.session_id,
                "list_scheduled_tasks.session_lookup_failed");
            ToolError::Backend(format!("list_scheduled_tasks: session lookup: {e}"))
        })?;
        let mut tx = begin_as_user(&self.pool, tenancy.created_by_user_id)
            .await
            .map_err(|e| {
                warn!(error = %e, "list_scheduled_tasks.begin_as_user_failed");
                ToolError::Backend(format!("list_scheduled_tasks: tx: {e}"))
            })?;
        // Existence-only probe; `EXISTS` keeps the result as `bool` so the
        // function doesn't traffic in raw `uuid::Uuid` (CLAUDE.md §1: IDs
        // are newtypes — a bare `Uuid` in app code is a review-blocker).
        let visible: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM agents WHERE id = $1)")
            .bind(owner)
            .fetch_one(&mut *tx)
            .await
            .map_err(|e| {
                warn!(error = %e, patom.agent.id = %owner,
                            "list_scheduled_tasks.visibility_check_failed");
                ToolError::Backend(format!("list_scheduled_tasks: visibility: {e}"))
            })?;
        drop(tx);
        if !visible {
            return Ok(serde_json::to_string(&Output { tasks: Vec::new() })?);
        }

        let rows = self.store.list_for_agent(owner).await.map_err(|e| {
            warn!(error = %e, patom.agent.id = %owner, "list_scheduled_tasks.failed");
            ToolError::Backend(format!("list_scheduled_tasks: {e}"))
        })?;
        let out = Output {
            tasks: rows.into_iter().map(ListedTask::from).collect(),
        };
        Ok(serde_json::to_string(&out)?)
    }
}
