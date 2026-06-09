//! `cancel_scheduled_task` — cancel one of the caller's scheduled tasks.

use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::PgPool;
use tracing::{info, warn};

use crate::auth::begin_as_user;
use crate::scheduling::{ScheduledTaskError, ScheduledTaskId, SharedScheduledTaskStore};
use crate::tools::{Tool, ToolCallContext, ToolError};
use crate::types::ToolName;

const TOOL_NAME: &str = "cancel_scheduled_task";
const TOOL_DESCRIPTION: &str = "Cancel one of your scheduled tasks. You can only \
    cancel tasks you yourself scheduled. Cancelling an already-cancelled or completed task \
    is a no-op.\n\
    \n\
    Argument: `task_id` from `list_scheduled_tasks` (or the id you remembered when you \
    created the task).";

#[derive(Debug, Deserialize)]
struct Input {
    task_id: ScheduledTaskId,
}

#[derive(Debug, Serialize)]
struct Output {
    task_id: ScheduledTaskId,
}

pub struct CancelScheduledTaskTool {
    name: ToolName,
    description: &'static str,
    input_schema: Arc<Value>,
    store: SharedScheduledTaskStore,
    pool: PgPool,
}

impl std::fmt::Debug for CancelScheduledTaskTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CancelScheduledTaskTool")
            .finish_non_exhaustive()
    }
}

impl CancelScheduledTaskTool {
    #[must_use]
    pub fn new(store: SharedScheduledTaskStore, pool: PgPool) -> Self {
        let name =
            ToolName::try_from(TOOL_NAME).expect("invariant: cancel_scheduled_task valid name");
        let input_schema = Arc::new(json!({
            "type": "object",
            "required": ["task_id"],
            "properties": {
                "task_id": { "type": "string", "format": "uuid" }
            },
            "additionalProperties": false
        }));
        Self {
            name,
            description: TOOL_DESCRIPTION,
            input_schema,
            store,
            pool,
        }
    }
}

#[async_trait]
impl Tool for CancelScheduledTaskTool {
    fn name(&self) -> &ToolName {
        &self.name
    }
    fn description(&self) -> &str {
        self.description
    }
    fn input_schema(&self) -> Arc<Value> {
        self.input_schema.clone()
    }

    #[tracing::instrument(
        skip_all,
        name = "tool.cancel_scheduled_task",
        fields(patom.from.viewer = %ctx.viewer),
    )]
    async fn execute(&self, input: Value, ctx: &ToolCallContext) -> Result<String, ToolError> {
        let parsed: Input = serde_json::from_value(input)?;
        let owner = ctx.viewer.agent_id().ok_or_else(|| {
            ToolError::InvalidInput("cancel_scheduled_task: caller must be an agent".into())
        })?;

        // Tenant-side gate: a privileged store would happily let a
        // misrouted call delete a row in another org if the caller
        // somehow guessed both `(task_id, owner_agent_id)`. Open a
        // `begin_as_user` tx pinned to the acting principal (the human at
        // the DAG root, off the tool-call context) and confirm the row is
        // visible under RLS before delegating to the privileged-tx store;
        // cross-tenant calls see zero rows here and fold into `NotFound`
        // exactly like the cross-owner case.
        let mut tx = begin_as_user(&self.pool, ctx.acting_user_id)
            .await
            .map_err(|e| {
                warn!(error = %e, "cancel_scheduled_task.begin_as_user_failed");
                ToolError::Backend(format!("cancel_scheduled_task: tx: {e}"))
            })?;
        // Existence-only probe under RLS; `EXISTS` returns bool, keeping
        // raw `uuid::Uuid` out of app code (CLAUDE.md §1).
        let visible: bool = sqlx::query_scalar(
            "SELECT EXISTS(\
                SELECT 1 FROM scheduled_tasks \
                WHERE id = $1 AND owner_agent_id = $2\
             )",
        )
        .bind(parsed.task_id)
        .bind(owner)
        .fetch_one(&mut *tx)
        .await
        .map_err(|e| {
            warn!(error = %e, patom.scheduled_task.id = %parsed.task_id,
                "cancel_scheduled_task.visibility_check_failed");
            ToolError::Backend(format!("cancel_scheduled_task: visibility: {e}"))
        })?;
        drop(tx);
        if !visible {
            return Err(ToolError::InvalidInput(format!(
                "cancel_scheduled_task: task {} not found",
                parsed.task_id
            )));
        }

        match self
            .store
            .cancel_for_user(ctx.acting_user_id, parsed.task_id, owner)
            .await
        {
            Ok(()) => {
                info!(
                    patom.scheduled_task.id = %parsed.task_id,
                    patom.agent.id = %owner,
                    "cancel_scheduled_task.cancelled",
                );
                Ok(serde_json::to_string(&Output {
                    task_id: parsed.task_id,
                })?)
            }
            // `cancel` folds the cross-owner case into `NotFound` so the
            // tool seam cannot be used to probe for other agents' rows.
            Err(ScheduledTaskError::NotFound(_)) => Err(ToolError::InvalidInput(format!(
                "cancel_scheduled_task: task {} not found",
                parsed.task_id
            ))),
            Err(e) => {
                warn!(error = %e, patom.scheduled_task.id = %parsed.task_id,
                    "cancel_scheduled_task.failed");
                Err(ToolError::Backend(format!("cancel_scheduled_task: {e}")))
            }
        }
    }
}
