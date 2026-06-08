//! Read/cancel HTTP surface for an agent's scheduled tasks.
//!
//! `GET  /agents/{id}/scheduled-tasks`        — paginated list + status rollup
//! `POST /agents/{id}/scheduled-tasks/{tid}/cancel` — cancel one task
//!
//! The scheduling subsystem is otherwise driven by agent tools
//! (`schedule_task` / `cancel_scheduled_task`) during conversations; this
//! module is the operator-facing view that powers the FE "Scheduled
//! Tasks" tab. Both handlers gate the agent id through [`visible_to`] so a
//! foreign / unknown id 404s with the same shape as `read_agent`, then run
//! RLS-scoped SQL under [`begin_as`] (the `scheduled_tasks_org_isolation`
//! policy filters to the principal's org).
//!
//! Schedule cadence and run instants are formatted into display strings
//! here — the FE renders them verbatim, so the wire contract carries
//! `schedule_label` / `next_run_label` rather than the structured
//! [`ScheduleSpec`] the model boundary speaks.

use axum::extract::{Path, Query, State};
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::agents::AgentId;
use crate::auth::{AuthError, Principal, VisibilityTable, begin_as, visible_to};
use crate::scheduling::{
    DEFAULT_SCHEDULED_TASKS_PAGE, MAX_SCHEDULED_TASKS_PAGE, ScheduleSpec, ScheduledTaskId,
    ScheduledTaskState, Timezone, Weekday, Weekdays,
};

use super::super::error::HttpError;
use super::super::state::AppState;

pub(super) fn router() -> Router<AppState> {
    Router::new()
        .route("/agents/{id}/scheduled-tasks", get(list_scheduled_tasks))
        .route(
            "/agents/{id}/scheduled-tasks/{task_id}/cancel",
            post(cancel_scheduled_task),
        )
}

// ─── wire shapes ─────────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
struct ScheduledTaskResponse {
    id: ScheduledTaskId,
    agent_id: AgentId,
    agent_name: String,
    name: String,
    /// `active` | `completed` | `cancelled` — the FE's tri-state. Maps the
    /// store's `done` to `completed`.
    status: &'static str,
    /// `recurring` | `one_time` — derived from the schedule variant.
    kind: &'static str,
    /// Compact cadence, e.g. `"Every Mon, Wed, 09:00 UTC"`.
    schedule_label: String,
    /// Verbose cadence for the cancel dialog, e.g.
    /// `"Every weekday at 14:00 UTC"`.
    schedule_full: String,
    /// Next-fire instant (UTC) for a live task, else `null`.
    next_run_label: Option<String>,
    /// Last-fire instant (UTC), or `null` when the task has never run.
    last_run_label: Option<String>,
}

#[derive(Debug, Serialize)]
struct ScheduledTaskSummary {
    active: u32,
    completed: u32,
    cancelled: u32,
}

#[derive(Debug, Serialize)]
struct ScheduledTaskListResponse {
    items: Vec<ScheduledTaskResponse>,
    total: u32,
    summary: ScheduledTaskSummary,
}

#[derive(Debug, Deserialize)]
struct ListQuery {
    page: Option<u32>,
    per_page: Option<u32>,
}

/// One row of the listing/cancel SELECT. The two `name` columns are
/// aliased (`agent_name`, `task_name`) so `FromRow` can disambiguate them.
#[derive(sqlx::FromRow)]
struct ListRow {
    id: ScheduledTaskId,
    owner_agent_id: AgentId,
    agent_name: String,
    task_name: String,
    schedule: serde_json::Value,
    next_run_at: Option<DateTime<Utc>>,
    last_fired_at: Option<DateTime<Utc>>,
    state: ScheduledTaskState,
}

impl ListRow {
    fn into_response(self) -> Result<ScheduledTaskResponse, HttpError> {
        // The `schedule` column round-trips through the same `ScheduleSpec`
        // the tool boundary wrote; a decode failure here means a poisoned
        // row, not bad input — surface it as a 500.
        let spec: ScheduleSpec =
            serde_json::from_value(self.schedule).map_err(|_| HttpError::Internal)?;
        let (schedule_label, schedule_full) = schedule_labels(&spec);
        // Next-run only reads meaningfully for a live task; done / cancelled
        // rows render an em dash on the FE, so null the label rather than
        // ship a stale instant.
        let next_run_label = match self.state {
            ScheduledTaskState::Active => self.next_run_at.map(fmt_instant),
            ScheduledTaskState::Done | ScheduledTaskState::Cancelled => None,
        };
        Ok(ScheduledTaskResponse {
            id: self.id,
            agent_id: self.owner_agent_id,
            agent_name: self.agent_name,
            name: self.task_name,
            status: status_str(self.state),
            kind: kind_str(&spec),
            schedule_label,
            schedule_full,
            next_run_label,
            last_run_label: self.last_fired_at.map(fmt_instant),
        })
    }
}

// ─── handlers ────────────────────────────────────────────────────────────

async fn list_scheduled_tasks(
    State(state): State<AppState>,
    principal: Principal,
    Path(id): Path<Uuid>,
    Query(params): Query<ListQuery>,
) -> Result<Json<ScheduledTaskListResponse>, HttpError> {
    let agent_id = AgentId::from(id);
    if !visible_to(&state.pool, &principal, VisibilityTable::Agents, id).await? {
        return Err(HttpError::NotFound);
    }

    let per_page = params
        .per_page
        .unwrap_or(DEFAULT_SCHEDULED_TASKS_PAGE)
        .clamp(1, MAX_SCHEDULED_TASKS_PAGE);
    let page = params.page.unwrap_or(1).max(1);
    let offset = i64::from(page - 1) * i64::from(per_page);
    assert!(per_page >= 1, "invariant: per_page clamped to >= 1");
    assert!(offset >= 0, "invariant: offset is non-negative");

    let mut tx = begin_as(&state.pool, &principal).await?;
    let counts = sqlx::query_as::<_, (ScheduledTaskState, i64)>(
        "SELECT state, COUNT(*) FROM scheduled_tasks \
         WHERE owner_agent_id = $1 GROUP BY state",
    )
    .bind(agent_id)
    .fetch_all(&mut *tx)
    .await
    .map_err(AuthError::from)?;
    let rows = sqlx::query_as::<_, ListRow>(
        "SELECT st.id, st.owner_agent_id, a.name AS agent_name, st.name AS task_name, \
                st.schedule, st.next_run_at, st.last_fired_at, st.state \
         FROM scheduled_tasks st \
         JOIN agents a ON a.id = st.owner_agent_id \
         WHERE st.owner_agent_id = $1 \
         ORDER BY st.created_at ASC \
         LIMIT $2 OFFSET $3",
    )
    .bind(agent_id)
    .bind(i64::from(per_page))
    .bind(offset)
    .fetch_all(&mut *tx)
    .await
    .map_err(AuthError::from)?;
    tx.commit().await.map_err(AuthError::from)?;

    let summary = summarize(&counts);
    let total = summary.active + summary.completed + summary.cancelled;
    let items = rows
        .into_iter()
        .map(ListRow::into_response)
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Json(ScheduledTaskListResponse {
        items,
        total,
        summary,
    }))
}

async fn cancel_scheduled_task(
    State(state): State<AppState>,
    principal: Principal,
    Path((id, task_id)): Path<(Uuid, Uuid)>,
) -> Result<Json<ScheduledTaskResponse>, HttpError> {
    let agent_id = AgentId::from(id);
    let task = ScheduledTaskId::from(task_id);
    if !visible_to(&state.pool, &principal, VisibilityTable::Agents, id).await? {
        return Err(HttpError::NotFound);
    }

    let now = state.clock.now_utc();
    let mut tx = begin_as(&state.pool, &principal).await?;
    // Idempotent: re-cancelling a cancelled row re-sets the same state and
    // returns it. `next_run_at = NULL` stops the scheduler re-firing and
    // makes the FE render the next-run dash. Ownership is enforced by the
    // `owner_agent_id` predicate (plus RLS on the org), so a foreign /
    // unknown task updates zero rows and 404s below.
    let row = sqlx::query_as::<_, ListRow>(
        "WITH updated AS ( \
            UPDATE scheduled_tasks \
            SET state = 'cancelled', next_run_at = NULL, updated_at = $3 \
            WHERE id = $1 AND owner_agent_id = $2 \
            RETURNING id, owner_agent_id, name, schedule, next_run_at, last_fired_at, state \
         ) \
         SELECT u.id, u.owner_agent_id, a.name AS agent_name, u.name AS task_name, \
                u.schedule, u.next_run_at, u.last_fired_at, u.state \
         FROM updated u JOIN agents a ON a.id = u.owner_agent_id",
    )
    .bind(task)
    .bind(agent_id)
    .bind(now)
    .fetch_optional(&mut *tx)
    .await
    .map_err(AuthError::from)?;
    tx.commit().await.map_err(AuthError::from)?;

    let row = row.ok_or(HttpError::NotFound)?;
    assert_eq!(
        row.owner_agent_id, agent_id,
        "invariant: cancel returned another agent's task"
    );
    assert!(
        matches!(row.state, ScheduledTaskState::Cancelled),
        "invariant: cancel left the task in a non-cancelled state"
    );
    Ok(Json(row.into_response()?))
}

// ─── formatting helpers ──────────────────────────────────────────────────

/// Fold the per-state counts into the FE rollup. Unknown states cannot
/// occur (the column has a `CHECK`), so the match is total over the enum.
fn summarize(counts: &[(ScheduledTaskState, i64)]) -> ScheduledTaskSummary {
    let mut summary = ScheduledTaskSummary {
        active: 0,
        completed: 0,
        cancelled: 0,
    };
    for (state, n) in counts {
        // COUNT(*) is non-negative and bounded by the table size; clamp
        // rather than panic on the impossible negative.
        let n = u32::try_from(*n).unwrap_or(u32::MAX);
        match state {
            ScheduledTaskState::Active => summary.active = n,
            ScheduledTaskState::Done => summary.completed = n,
            ScheduledTaskState::Cancelled => summary.cancelled = n,
        }
    }
    summary
}

fn status_str(state: ScheduledTaskState) -> &'static str {
    match state {
        ScheduledTaskState::Active => "active",
        ScheduledTaskState::Done => "completed",
        ScheduledTaskState::Cancelled => "cancelled",
    }
}

fn kind_str(spec: &ScheduleSpec) -> &'static str {
    match spec {
        ScheduleSpec::Once { .. } => "one_time",
        ScheduleSpec::Recurring { .. } => "recurring",
    }
}

/// `"Mon Jun 09, 09:00"` in UTC — stable and locale-independent so the
/// FE never has to re-parse a localized string.
fn fmt_instant(at: DateTime<Utc>) -> String {
    at.format("%a %b %d, %H:%M").to_string()
}

/// `(compact, verbose)` cadence labels for one schedule.
fn schedule_labels(spec: &ScheduleSpec) -> (String, String) {
    match spec {
        ScheduleSpec::Once { run_at } => {
            let when = run_at.format("%b %d, %H:%M").to_string();
            (format!("Once: {when} UTC"), format!("Once at {when} UTC"))
        }
        ScheduleSpec::Recurring { weekdays, time, tz } => {
            let days = days_label(*weekdays);
            let suffix = tz_suffix(*tz);
            (
                format!("Every {days}, {time} {suffix}"),
                format!("Every {days} at {time} {suffix}"),
            )
        }
    }
}

/// Human day-set label: the named common sets collapse to a word, an
/// arbitrary set lists its abbreviations (`"Mon, Wed, Fri"`).
fn days_label(w: Weekdays) -> String {
    if w == Weekdays::ALL {
        return "day".to_string();
    }
    if w == Weekdays::WORKDAYS {
        return "weekday".to_string();
    }
    if w == Weekdays::WEEKENDS {
        return "weekend".to_string();
    }
    w.iter().map(weekday_short).collect::<Vec<_>>().join(", ")
}

fn weekday_short(d: Weekday) -> &'static str {
    match d {
        Weekday::Mon => "Mon",
        Weekday::Tue => "Tue",
        Weekday::Wed => "Wed",
        Weekday::Thu => "Thu",
        Weekday::Fri => "Fri",
        Weekday::Sat => "Sat",
        Weekday::Sun => "Sun",
    }
}

/// `"UTC"` for UTC, `"(Area/City)"` for any other IANA zone — matches the
/// cadence-label phrasing the FE mock established.
fn tz_suffix(tz: Timezone) -> String {
    let name = tz.name();
    if name == "UTC" {
        "UTC".to_string()
    } else {
        format!("({name})")
    }
}
