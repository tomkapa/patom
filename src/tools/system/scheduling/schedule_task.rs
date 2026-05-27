//! `schedule_task` — register a future wake-up turn.

use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tracing::{error, info, warn};

use crate::agents::SharedAgentStore;
use crate::clock::SharedClock;
use crate::scheduling::{
    DefaultTimezone, MAX_ONESHOT_HORIZON_DAYS, NewScheduledTask, SCHEDULED_TASK_NAME_MAX_LEN,
    ScheduleSpec, ScheduledPrompt, ScheduledTaskError, ScheduledTaskId, ScheduledTaskName,
    SharedScheduledTaskStore,
};
use crate::session::SharedSessionStore;
use crate::tools::{Tool, ToolCallContext, ToolError};
use crate::types::{PROMPT_MAX_BYTES, ToolName};

const TOOL_NAME: &str = "schedule_task";
const TOOL_DESCRIPTION: &str = "Register a future wake-up. The system will fire the \
    given prompt at the scheduled time(s) as a fresh conversation turn for you, and you can \
    reply through `send_message` like any normal turn.\n\
    \n\
    Two schedule shapes:\n\
    - `once`: fires a single time at `run_at` (ISO-8601 UTC).\n\
    - `recurring`: fires on each weekday in `weekdays` at `time` (HH:MM) in `tz` (IANA name).\n\
      Covers daily, workdays, weekly, weekends — set `weekdays` to the days you want.\n\
    \n\
    Confirm the timezone with the user when they didn't say (it's not enough to assume \
    UTC). Confirm the action with the user before scheduling unless they explicitly asked \
    for it. Use `list_scheduled_tasks` to see what's already set, and `cancel_scheduled_task` \
    to remove a task.\n\
    \n\
    Arguments: `name` (short label, ≤200 bytes), `prompt` (the body the system will fire \
    at you on each wake-up; write it as if a user is asking you to do the task), \
    `schedule` (the `once` or `recurring` shape above; `tz` is optional on `recurring` and \
    defaults to a system-configured timezone).";

#[derive(Debug, Deserialize)]
struct Input {
    name: String,
    prompt: String,
    schedule: ScheduleInput,
}

/// Wire-side schedule shape — accepts `tz` as optional on `recurring`.
/// The tool fills the resolver's default before calling `next_after`,
/// so the stored row's `tz` is always Some.
#[derive(Debug, Deserialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
enum ScheduleInput {
    Once {
        run_at: DateTime<Utc>,
    },
    Recurring {
        weekdays: crate::scheduling::Weekdays,
        time: crate::scheduling::TimeOfDay,
        #[serde(default)]
        tz: Option<crate::scheduling::Timezone>,
    },
}

#[derive(Debug, Serialize)]
struct Output {
    task_id: ScheduledTaskId,
    name: ScheduledTaskName,
    next_run_at: DateTime<Utc>,
}

pub struct ScheduleTaskTool {
    name: ToolName,
    description: &'static str,
    input_schema: Arc<Value>,
    store: SharedScheduledTaskStore,
    agents: SharedAgentStore,
    sessions: SharedSessionStore,
    default_tz: DefaultTimezone,
    clock: SharedClock,
}

impl std::fmt::Debug for ScheduleTaskTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ScheduleTaskTool").finish_non_exhaustive()
    }
}

impl ScheduleTaskTool {
    #[must_use]
    pub fn new(
        store: SharedScheduledTaskStore,
        agents: SharedAgentStore,
        sessions: SharedSessionStore,
        default_tz: DefaultTimezone,
        clock: SharedClock,
    ) -> Self {
        let name = ToolName::try_from(TOOL_NAME).expect("invariant: schedule_task valid name");
        let input_schema = Arc::new(json!({
            "type": "object",
            "required": ["name", "prompt", "schedule"],
            "properties": {
                "name": { "type": "string", "minLength": 1, "maxLength": SCHEDULED_TASK_NAME_MAX_LEN },
                "prompt": { "type": "string", "minLength": 1, "maxLength": PROMPT_MAX_BYTES },
                "schedule": {
                    "oneOf": [
                        {
                            "type": "object",
                            "required": ["kind", "data"],
                            "properties": {
                                "kind": { "const": "once" },
                                "data": {
                                    "type": "object",
                                    "required": ["run_at"],
                                    "properties": {
                                        "run_at": { "type": "string", "format": "date-time" }
                                    },
                                    "additionalProperties": false
                                }
                            },
                            "additionalProperties": false
                        },
                        {
                            "type": "object",
                            "required": ["kind", "data"],
                            "properties": {
                                "kind": { "const": "recurring" },
                                "data": {
                                    "type": "object",
                                    "required": ["weekdays", "time"],
                                    "properties": {
                                        "weekdays": {
                                            "type": "array",
                                            "minItems": 1,
                                            "uniqueItems": true,
                                            "items": {
                                                "type": "string",
                                                "enum": ["mon","tue","wed","thu","fri","sat","sun"]
                                            }
                                        },
                                        "time": { "type": "string", "pattern": "^([01][0-9]|2[0-3]):[0-5][0-9]$" },
                                        "tz": { "type": ["string","null"] }
                                    },
                                    "additionalProperties": false
                                }
                            },
                            "additionalProperties": false
                        }
                    ]
                }
            },
            "additionalProperties": false
        }));
        Self {
            name,
            description: TOOL_DESCRIPTION,
            input_schema,
            store,
            agents,
            sessions,
            default_tz,
            clock,
        }
    }

    #[tracing::instrument(
        skip_all,
        name = "tool.schedule_task",
        fields(
            patom.from.viewer = %ctx.viewer,
            patom.scheduled_task.outcome = tracing::field::Empty,
            patom.scheduled_task.id = tracing::field::Empty,
        ),
    )]
    async fn handle(&self, input: Input, ctx: &ToolCallContext) -> Result<Output, ToolError> {
        let owner = ctx.viewer.agent_id().ok_or_else(|| {
            set_outcome(TaskOutcome::InvalidInput);
            ToolError::InvalidInput("schedule_task: caller must be an agent".into())
        })?;

        let name = ScheduledTaskName::try_from(input.name).map_err(|e| {
            set_outcome(TaskOutcome::InvalidInput);
            ToolError::InvalidInput(format!("schedule_task: name: {e}"))
        })?;
        let prompt = ScheduledPrompt::try_from(input.prompt).map_err(|e| {
            set_outcome(TaskOutcome::InvalidInput);
            ToolError::InvalidInput(format!("schedule_task: prompt: {e}"))
        })?;
        let schedule = self.materialize_schedule(input.schedule, owner);

        let now: DateTime<Utc> = self.clock.now_wall().into();
        self.assert_horizon(&schedule, now)?;
        let next_run_at = schedule.next_after(now).ok_or_else(|| {
            set_outcome(TaskOutcome::NoFutureFire);
            ToolError::InvalidInput("schedule_task: schedule produces no future fire time".into())
        })?;

        // Tenancy: `org_id` is the calling agent's org (every agent has
        // a NOT NULL `agents.org_id` — single source of truth for which
        // tenant the scheduled task belongs to). `created_by_user_id`
        // is the human at the DAG root of this session, fetched via
        // `SessionStore::tenancy` so a retried fire resolves the same
        // principal even if the agent loop has churned through several
        // sessions in the interim.
        let agent_record = self.agents.read(owner).await.map_err(|e| {
            set_outcome(TaskOutcome::BackendError);
            error!(error = ?e, patom.agent.id = %owner, "schedule_task.agent_lookup_failed");
            ToolError::Backend(format!("schedule_task: agent lookup: {e}"))
        })?;
        let session_tenancy = self.sessions.tenancy(ctx.session_id).await.map_err(|e| {
            set_outcome(TaskOutcome::BackendError);
            error!(error = ?e, patom.session.id = %ctx.session_id,
                "schedule_task.session_lookup_failed");
            ToolError::Backend(format!("schedule_task: session lookup: {e}"))
        })?;

        let payload = NewScheduledTask {
            owner_agent_id: owner,
            org_id: agent_record.org_id,
            created_by_user_id: session_tenancy.created_by_user_id,
            name,
            prompt,
            schedule,
            next_run_at: Some(next_run_at),
        };
        // Authorise + persist as the session-derived principal (the
        // human at the DAG root). `ctx.acting_user_id` and
        // `session_tenancy.created_by_user_id` agree under normal
        // worker dispatch, but if the call ever arrives via a path
        // where they diverge the row must commit under the same
        // principal that the fire-tx will later assume — otherwise the
        // task auths under one user and executes under another.
        let row = match self
            .store
            .create_for_user(session_tenancy.created_by_user_id, payload)
            .await
        {
            Ok(row) => row,
            Err(ScheduledTaskError::PerAgentCapExceeded { max, .. }) => {
                set_outcome(TaskOutcome::CapExceeded);
                return Err(ToolError::InvalidInput(format!(
                    "schedule_task: per-agent cap {max} reached; \
                     cancel an existing task before scheduling a new one"
                )));
            }
            Err(e) => {
                set_outcome(TaskOutcome::BackendError);
                warn!(error = %e, patom.agent.id = %owner, "schedule_task.create_failed");
                return Err(ToolError::Backend(format!(
                    "schedule_task: create failed: {e}"
                )));
            }
        };

        tracing::Span::current().record("patom.scheduled_task.id", tracing::field::display(row.id));
        set_outcome(TaskOutcome::Created);
        info!(
            patom.scheduled_task.id = %row.id,
            patom.agent.id = %owner,
            patom.scheduled_task.next_run_at = %next_run_at,
            "schedule_task.created",
        );
        Ok(Output {
            task_id: row.id,
            name: row.name,
            next_run_at,
        })
    }

    /// Convert the wire-side input to the canonical [`ScheduleSpec`].
    /// Default-tz resolution happens here so the stored row carries an
    /// explicit IANA name.
    fn materialize_schedule(
        &self,
        input: ScheduleInput,
        owner: crate::agents::AgentId,
    ) -> ScheduleSpec {
        match input {
            ScheduleInput::Once { run_at } => ScheduleSpec::Once { run_at },
            ScheduleInput::Recurring { weekdays, time, tz } => {
                let tz = tz.unwrap_or_else(|| self.default_tz.for_agent(owner));
                ScheduleSpec::Recurring { weekdays, time, tz }
            }
        }
    }

    /// Reject a `Once` further out than [`MAX_ONESHOT_HORIZON_DAYS`].
    /// `Recurring` has no horizon — the cadence is unbounded by design.
    fn assert_horizon(&self, schedule: &ScheduleSpec, now: DateTime<Utc>) -> Result<(), ToolError> {
        if let ScheduleSpec::Once { run_at } = schedule {
            let delta = *run_at - now;
            let days = delta.num_days();
            if delta > Duration::days(MAX_ONESHOT_HORIZON_DAYS) {
                set_outcome(TaskOutcome::HorizonExceeded);
                return Err(ToolError::InvalidInput(format!(
                    "schedule_task: oneshot {days} days in the future exceeds horizon of \
                     {MAX_ONESHOT_HORIZON_DAYS} days"
                )));
            }
        }
        Ok(())
    }
}

#[async_trait]
impl Tool for ScheduleTaskTool {
    fn name(&self) -> &ToolName {
        &self.name
    }
    fn description(&self) -> &str {
        self.description
    }
    fn input_schema(&self) -> Arc<Value> {
        self.input_schema.clone()
    }
    async fn execute(&self, input: Value, ctx: &ToolCallContext) -> Result<String, ToolError> {
        let parsed: Input = serde_json::from_value(input)?;
        let out = self.handle(parsed, ctx).await?;
        Ok(serde_json::to_string(&out)?)
    }
}

/// Outcome label recorded on the `tool.schedule_task` span. Mirrors
/// `send_message`'s pattern so dashboards `GROUP BY outcome` directly;
/// enum + typed setter prevents drift between call sites.
#[derive(Debug, Clone, Copy)]
enum TaskOutcome {
    Created,
    InvalidInput,
    NoFutureFire,
    HorizonExceeded,
    CapExceeded,
    BackendError,
}

impl TaskOutcome {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Created => "created",
            Self::InvalidInput => "invalid_input",
            Self::NoFutureFire => "no_future_fire",
            Self::HorizonExceeded => "horizon_exceeded",
            Self::CapExceeded => "cap_exceeded",
            Self::BackendError => "backend_error",
        }
    }
}

fn set_outcome(outcome: TaskOutcome) {
    tracing::Span::current().record("patom.scheduled_task.outcome", outcome.as_str());
}
