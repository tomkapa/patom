//! Per-turn detail endpoint (doc/logs_metrics_tab.md §5.4).
//!
//! `GET /turns/:turn_id` returns everything the Logs & Metrics drawer
//! needs in one shot — the `turn_metrics` row, the reasoning blocks the
//! assistant emitted, the tool calls it dispatched, the memory writes it
//! produced, and the `agent_prompt_versions` snapshot it ran on. Bounded
//! per CLAUDE.md §5: every join hits a fixed ceiling
//! (`MAX_TOOL_CALLS_PER_TURN`, `MAX_HOOKS_PER_TURN`,
//! `MAX_REASONING_BLOCKS_PER_TURN`, `MAX_MEMORY_WRITES_PER_TURN`).
//!
//! The path key is the per-row `turn_metrics.id`, so a multi-turn reply's
//! turns are addressed individually (several rows share one `request_id`).
//! The metrics header is therefore per-turn-exact. The sub-resources
//! (reasoning, tool calls, memory writes) remain scoped by the turn's
//! `request_id`: the schema relates those tables to the *request*, not to
//! an individual provider call, so a per-turn split would need a separate
//! migration. They are request-complete, not per-turn.
//!
//! Tenant safety lives at two layers: `visible_to(TurnMetrics, …)` 404s
//! cross-org / unknown ids before we open the inner tx, and the inner tx
//! runs `begin_as(principal)` so every join is RLS-filtered.

use axum::Json;
use axum::Router;
use axum::extract::{Path, State};
use axum::routing::get;
use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::Value as JsonValue;
use sqlx::types::Json as SqlxJson;
use thiserror::Error;
use uuid::Uuid;

use crate::agent_core::MAX_TOOL_CALLS_PER_TURN;
use crate::agent_core::turn_metrics::TurnMetricsId;
use crate::agents::AgentId;
use crate::agents::prompt_versions::PromptVersionId;
use crate::auth::{AuthError, Principal, UserId, VisibilityTable, visible_to};
use crate::mcp::McpServerId;
use crate::memory::{MemoryEventId, MemoryId};
use crate::provider::{Model, ProviderId};
use crate::runtime::{ClaimKey, PromptRequestId, RequestKind};
use crate::tools::ToolCallRowId;
// MessageSenderKind was the old wire-typed enum; after Stage 3 the kind is
// joined from colleagues so the route binds the literal "agent" string.

use super::super::error::HttpError;
use super::super::state::AppState;

/// Hard cap on reasoning blocks returned for one turn. Each turn produces
/// a single assistant message in `session_messages`; in practice the cap
/// fires only if the content is split across multiple rows (e.g. a
/// retried turn). The drawer renders the lot inline, so a runaway count
/// would bloat the response without adding signal.
const MAX_REASONING_BLOCKS_PER_TURN: usize = 32;

/// Hard cap on memory writes attributed to one turn. The librarian's
/// per-turn upper bound is well under this — the cap defends the join
/// against a future loop bug, not against expected traffic.
const MAX_MEMORY_WRITES_PER_TURN: usize = 64;

pub(super) fn router() -> Router<AppState> {
    Router::new().route("/turns/{turn_id}", get(read_turn_detail))
}

/// One error type for this module's boundary. CLAUDE.md §12: the route
/// converts every variant to an `HttpError`, the only type the axum
/// glue understands.
#[derive(Debug, Error)]
pub enum TurnDetailError {
    /// The turn id was not visible to the caller's principal — either it
    /// doesn't exist or it belongs to another org. Maps to 404 (we don't
    /// leak existence across orgs). The id *is* the `turn_metrics` row, so
    /// there is no separate "metrics not recorded yet" state: an id that
    /// resolves has a row by construction.
    #[error("turn {0} not found")]
    NotFound(TurnMetricsId),
    /// `turn_metrics.prompt_version_id` references a row that
    /// `fetch_prompt_version` couldn't resolve. Should be impossible
    /// under the FK constraint; logged loudly and 404'd so a single
    /// corrupt row can't take down the worker.
    #[error("prompt version {0} not found for turn")]
    PromptVersionMissing(PromptVersionId),
    /// `begin_as` / `visible_to` failure — JWT, membership, or the GUC
    /// set on the inner tx. Genuinely auth-flavoured: maps onto the
    /// existing `HttpError::Auth` matrix (401 / 403 / 500-"auth error").
    #[error(transparent)]
    Auth(#[from] AuthError),
    /// Inner sqlx query failure inside the tx — not an auth concern.
    /// Kept separate so the wire surface 500s as "turn detail error"
    /// instead of "auth error", and so a future caller can match on it
    /// without going through the auth variant.
    #[error("db: {0}")]
    Db(#[from] sqlx::Error),
}

impl From<TurnDetailError> for HttpError {
    fn from(e: TurnDetailError) -> Self {
        match e {
            // Turn gone / prompt-version parent unresolvable both map to
            // 404 on the wire — the drawer treats them the same way.
            // PromptVersionMissing is logged inside `fetch_prompt_version`
            // so we don't double-log here.
            TurnDetailError::NotFound(_) | TurnDetailError::PromptVersionMissing(_) => {
                Self::NotFound
            }
            // Bridge each remaining variant to its own HttpError seat so
            // the wire body and the 5xx tracing log identify the route,
            // not the auth subsystem. `IntoResponse for HttpError` logs
            // the full variant tree on 5xx already.
            TurnDetailError::Auth(e) => Self::Auth(e),
            TurnDetailError::Db(e) => Self::TurnDetail(TurnDetailError::Db(e)),
        }
    }
}

// ─── wire types ────────────────────────────────────────────────────────

/// `turn_metrics` row, flattened to JSON. Mirrors the columns one-for-one
/// — the FE reads each field individually for the drawer header chips.
#[derive(Debug, Serialize, sqlx::FromRow)]
struct TurnMetricsResponse {
    /// Per-row primary key — what the drawer is keyed on. One per provider
    /// call; several share a `request_id` for a multi-turn reply.
    id: TurnMetricsId,
    request_id: PromptRequestId,
    /// Agent participation id (`agent_thread_state.id`) the turn ran under —
    /// the recorder FK that replaced the legacy `session_id`.
    state_id: ClaimKey,
    /// Root prompt request of the human-rooted DAG this turn belongs to.
    root_request_id: PromptRequestId,
    /// Thread the turn ran in — what the memory pane deep-links into the
    /// chat view (threads are keyed by `thread_id`, not request ids).
    /// `None` for background-cognition turns (no thread).
    thread_id: Option<crate::threads::ThreadId>,
    agent_id: AgentId,
    prompt_version_id: PromptVersionId,
    kind: RequestKind,
    model: Model,
    provider: ProviderId,
    input_tokens: i32,
    output_tokens: i32,
    cache_creation_tokens: Option<i32>,
    cache_read_tokens: Option<i32>,
    duration_ms: i32,
    stop_reason: String,
    started_at: DateTime<Utc>,
    created_at: DateTime<Utc>,
    /// Joined from `prompt_requests` so the drawer can show "failed
    /// because: …" without a second round-trip. Null when the request
    /// completed successfully.
    failure_reason: Option<String>,
}

/// One reasoning block extracted from `session_messages.body`. The body
/// is a `ChatMessage` envelope — we filter to agent senders and
/// pull every `AssistantContent::Reasoning(text)` block out, recording
/// the byte count so the drawer can show "REASONING · 4.2 KB" before the
/// user expands it.
#[derive(Debug, Serialize)]
struct ReasoningBlock {
    /// Reasoning text. Always present (the variant carries it inline).
    text: String,
    /// `text.len()` (UTF-8 bytes). Surfaced so the drawer can render the
    /// collapsed header without re-measuring on the client.
    byte_count: usize,
}

/// One `tool_calls` row, scoped to this turn. Reuses the same shape the
/// per-agent audit endpoint returns so the FE can render with the same
/// `ToolCallLine` molecule.
#[derive(Debug, Serialize, sqlx::FromRow)]
struct TurnToolCallResponse {
    id: ToolCallRowId,
    tool_name: String,
    /// LEFT JOIN — see `list_agent_tool_calls` for the same `ON DELETE
    /// SET NULL` reasoning.
    mcp_server_id: Option<McpServerId>,
    mcp_server_catalog_id: Option<String>,
    started_at: DateTime<Utc>,
    duration_ms: i32,
    is_error: bool,
    error_message: Option<String>,
}

/// One `memory_events` row attributed to this turn. The drawer renders
/// a `+N written · M forgotten` summary and an expandable list using
/// these fields directly — no separate aggregation endpoint.
#[derive(Debug, Serialize, sqlx::FromRow)]
struct TurnMemoryEventResponse {
    id: MemoryEventId,
    mutation: String,
    target_memory_id: MemoryId,
    content_before: Option<String>,
    content_after: Option<String>,
    created_at: DateTime<Utc>,
}

/// `agent_prompt_versions` snapshot — the answer to "what was the agent
/// running when this turn happened?" (doc §5.4). Read-only here; the
/// restore action lives on a separate endpoint (slice 3).
///
/// Model is intentionally absent — it lives on `agents.model`, not on
/// the version row (migration 43 doc: "Versions ONLY the system_prompt").
/// The drawer's MODEL chip reads from `TurnMetricsResponse.model` above.
#[derive(Debug, Serialize, sqlx::FromRow)]
struct PromptVersionSnapshot {
    id: PromptVersionId,
    version: i32,
    system_prompt: String,
    edited_by: Option<UserId>,
    created_at: DateTime<Utc>,
}

/// Wire payload for `GET /turns/:request_id`.
#[derive(Debug, Serialize)]
struct TurnDetailResponse {
    turn: TurnMetricsResponse,
    reasoning_blocks: Vec<ReasoningBlock>,
    tool_calls: Vec<TurnToolCallResponse>,
    memory_writes: Vec<TurnMemoryEventResponse>,
    prompt_version: PromptVersionSnapshot,
}

// ─── handler ───────────────────────────────────────────────────────────

async fn read_turn_detail(
    State(state): State<AppState>,
    principal: Principal,
    Path(turn_id): Path<Uuid>,
) -> Result<Json<TurnDetailResponse>, HttpError> {
    let turn_id = TurnMetricsId::from(turn_id);

    // Pre-gate: 404 cross-org / unknown turn ids with the same shape the
    // rest of the surface uses. Without this a foreign id would bubble up
    // as an inner-tx "row not found" — distinguishable from "no such turn"
    // by timing, which leaks existence.
    if !visible_to(
        &state.pool,
        &principal,
        VisibilityTable::TurnMetrics,
        turn_id.as_uuid(),
    )
    .await?
    {
        return Err(TurnDetailError::NotFound(turn_id).into());
    }

    let detail = load_detail(&state, &principal, turn_id).await?;
    Ok(Json(detail))
}

async fn load_detail(
    state: &AppState,
    principal: &Principal,
    turn_id: TurnMetricsId,
) -> Result<TurnDetailResponse, TurnDetailError> {
    let mut tx = crate::auth::begin_as(&state.pool, principal).await?;
    let turn = fetch_turn_row(&mut tx, turn_id).await?;
    // Sub-resources are request-scoped (see module docs): the tables relate
    // to the request, not the individual provider call. Thread the turn's
    // own `request_id` through so they fetch the right reply's activity.
    let request_id = turn.request_id;
    let reasoning_blocks = fetch_reasoning(&mut tx, request_id).await?;
    let tool_calls = fetch_tool_calls(&mut tx, request_id).await?;
    let memory_writes = fetch_memory_writes(&mut tx, request_id).await?;
    let prompt_version = fetch_prompt_version(&mut tx, turn.prompt_version_id).await?;
    tx.commit().await?;

    Ok(TurnDetailResponse {
        turn,
        reasoning_blocks,
        tool_calls,
        memory_writes,
        prompt_version,
    })
}

/// Type alias for the inner tx handle threaded through every per-section
/// helper below. The tx is opened in `load_detail` via `begin_as` and
/// committed there so every helper shares the same `app.user_id` GUC.
type TenantTx<'a> = sqlx::Transaction<'a, sqlx::Postgres>;

/// 1. `turn_metrics` + the parent `prompt_requests.failure_reason`.
///    One round-trip, keyed on the per-row `turn_metrics.id` so each turn
///    of a multi-turn reply resolves to its own metrics. The pre-gate
///    already confirmed visibility; a `None` here means the row vanished
///    between the gate and the tx (a delete race), which 404s the same way.
async fn fetch_turn_row(
    tx: &mut TenantTx<'_>,
    turn_id: TurnMetricsId,
) -> Result<TurnMetricsResponse, TurnDetailError> {
    let row = sqlx::query_as::<_, TurnMetricsResponse>(
        "SELECT tm.id, tm.request_id, tm.state_id, pr.root_request_id, \
                pr.thread_id, \
                tm.agent_id, tm.prompt_version_id, \
                tm.kind, tm.model, tm.provider, \
                tm.input_tokens, tm.output_tokens, \
                tm.cache_creation_tokens, tm.cache_read_tokens, \
                tm.duration_ms, tm.stop_reason, \
                tm.started_at, tm.created_at, \
                pr.failure_reason \
         FROM turn_metrics tm \
         JOIN prompt_requests pr ON pr.id = tm.request_id \
         WHERE tm.id = $1",
    )
    .bind(turn_id)
    .fetch_optional(&mut **tx)
    .await?;
    row.ok_or(TurnDetailError::NotFound(turn_id))
}

/// 2. Reasoning blocks from `session_messages`. CLAUDE.md §5: bounded
///    both by `MAX_REASONING_BLOCKS_PER_TURN` (cap on rows fetched) and
///    by the extraction loop (one row may carry many blocks; the loop
///    breaks once the cap is reached).
///
/// The body JSONB is a `ChatMessage` envelope —
/// `{"role": "assistant", "contents": [{"kind": "reasoning", "value": "…"}, …]}`.
/// We read the raw JSON and walk it here rather than deserializing into
/// the full `ChatMessage` enum, so a future content variant can't break
/// the drawer at runtime — unknown variants are skipped, not errored.
async fn fetch_reasoning(
    tx: &mut TenantTx<'_>,
    request_id: PromptRequestId,
) -> Result<Vec<ReasoningBlock>, TurnDetailError> {
    // Reasoning blocks live in the agent's owner-private feed artifacts
    // (`thread_messages` with `owner_agent_id` set) — the rehome of the old
    // `session_messages` assistant rows. Filter to the producing request.
    let bodies: Vec<(SqlxJson<JsonValue>,)> = sqlx::query_as(
        "SELECT tmsg.body FROM thread_messages tmsg \
         WHERE tmsg.request_id = $1 AND tmsg.owner_agent_id IS NOT NULL \
         ORDER BY tmsg.seq ASC \
         LIMIT $2",
    )
    .bind(request_id)
    .bind(i64::try_from(MAX_REASONING_BLOCKS_PER_TURN).unwrap_or(i64::MAX))
    .fetch_all(&mut **tx)
    .await?;
    let out = extract_reasoning(&bodies);
    assert!(
        out.len() <= MAX_REASONING_BLOCKS_PER_TURN,
        "invariant: reasoning blocks bounded by MAX_REASONING_BLOCKS_PER_TURN"
    );
    Ok(out)
}

/// 3. `tool_calls` bounded by `MAX_TOOL_CALLS_PER_TURN`.
async fn fetch_tool_calls(
    tx: &mut TenantTx<'_>,
    request_id: PromptRequestId,
) -> Result<Vec<TurnToolCallResponse>, TurnDetailError> {
    // `MAX_HOOKS_PER_TURN` (`agent_core::limits`) is the sibling cap
    // for hook events. It applies to the join `hook_events` will add in
    // slice 3 — we leave the constant in place but don't bind it here
    // because there's nothing to assert against yet.
    let rows = sqlx::query_as::<_, TurnToolCallResponse>(
        "SELECT tc.id, tc.tool_name, tc.mcp_server_id, s.catalog_id AS mcp_server_catalog_id, \
                tc.started_at, tc.duration_ms, tc.is_error, tc.error_message \
         FROM tool_calls tc \
         LEFT JOIN mcp_servers s ON s.id = tc.mcp_server_id \
         WHERE tc.request_id = $1 \
         ORDER BY tc.started_at ASC \
         LIMIT $2",
    )
    .bind(request_id)
    .bind(i64::try_from(MAX_TOOL_CALLS_PER_TURN).unwrap_or(i64::MAX))
    .fetch_all(&mut **tx)
    .await?;
    assert!(
        rows.len() <= MAX_TOOL_CALLS_PER_TURN,
        "invariant: LIMIT enforces MAX_TOOL_CALLS_PER_TURN ceiling"
    );
    Ok(rows)
}

/// 4. `memory_events` whose `source_turn_id` matches this turn.
async fn fetch_memory_writes(
    tx: &mut TenantTx<'_>,
    request_id: PromptRequestId,
) -> Result<Vec<TurnMemoryEventResponse>, TurnDetailError> {
    let rows = sqlx::query_as::<_, TurnMemoryEventResponse>(
        "SELECT id, mutation, target_memory_id, content_before, content_after, created_at \
         FROM memory_events \
         WHERE source_turn_id = $1 \
         ORDER BY created_at ASC, id ASC \
         LIMIT $2",
    )
    .bind(request_id)
    .bind(i64::try_from(MAX_MEMORY_WRITES_PER_TURN).unwrap_or(i64::MAX))
    .fetch_all(&mut **tx)
    .await?;
    assert!(
        rows.len() <= MAX_MEMORY_WRITES_PER_TURN,
        "invariant: LIMIT enforces MAX_MEMORY_WRITES_PER_TURN ceiling"
    );
    Ok(rows)
}

/// 5. The `agent_prompt_versions` snapshot for "what was the agent
///    running". The FK from `turn_metrics.prompt_version_id` plus the
///    append-only invariant on `agent_prompt_versions` make a missing
///    row genuinely impossible under a healthy schema. On the off chance
///    it happens (operator restored a partial backup, RLS hid the row
///    from the principal mid-tx), log loudly and 404 the whole drawer
///    rather than aborting the worker process.
async fn fetch_prompt_version(
    tx: &mut TenantTx<'_>,
    prompt_version_id: PromptVersionId,
) -> Result<PromptVersionSnapshot, TurnDetailError> {
    let row = sqlx::query_as::<_, PromptVersionSnapshot>(
        "SELECT id, version, system_prompt, edited_by, created_at \
         FROM agent_prompt_versions \
         WHERE id = $1",
    )
    .bind(prompt_version_id)
    .fetch_optional(&mut **tx)
    .await?;
    row.ok_or_else(|| {
        tracing::error!(
            patom.prompt_version.id = %prompt_version_id,
            "turn_detail.prompt_version.missing: turn_metrics row references an \
             unresolvable agent_prompt_versions parent",
        );
        TurnDetailError::PromptVersionMissing(prompt_version_id)
    })
}

/// Walk every assistant-message body and pull every `Reasoning` content
/// block out. Bounded by `MAX_REASONING_BLOCKS_PER_TURN` per CLAUDE.md
/// §5 — once we hit the cap we stop, even if more rows are pending.
///
/// The serde shape we read is `{kind: "reasoning", value: "…"}`,
/// matching `AssistantContent::Reasoning(String)` with serde tags
/// `tag = "kind", content = "value", rename_all = "snake_case"`.
fn extract_reasoning(bodies: &[(SqlxJson<JsonValue>,)]) -> Vec<ReasoningBlock> {
    let mut out = Vec::new();
    for (SqlxJson(body),) in bodies {
        let Some(contents) = body.get("contents").and_then(JsonValue::as_array) else {
            continue;
        };
        for entry in contents {
            if out.len() >= MAX_REASONING_BLOCKS_PER_TURN {
                return out;
            }
            let Some(kind) = entry.get("kind").and_then(JsonValue::as_str) else {
                continue;
            };
            if kind != "reasoning" {
                continue;
            }
            let Some(text) = entry.get("value").and_then(JsonValue::as_str) else {
                continue;
            };
            out.push(ReasoningBlock {
                text: text.to_owned(),
                byte_count: text.len(),
            });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn jsonb(v: JsonValue) -> (SqlxJson<JsonValue>,) {
        (SqlxJson(v),)
    }

    #[test]
    fn extract_reasoning_pulls_every_reasoning_block_in_order() {
        let bodies = vec![
            jsonb(json!({
                "role": "assistant",
                "contents": [
                    {"kind": "reasoning", "value": "first"},
                    {"kind": "text", "value": "skip me"},
                    {"kind": "reasoning", "value": "second"},
                ],
            })),
            jsonb(json!({
                "role": "assistant",
                "contents": [
                    {"kind": "reasoning", "value": "third"},
                ],
            })),
        ];
        let out = extract_reasoning(&bodies);
        assert_eq!(out.len(), 3);
        assert_eq!(out[0].text, "first");
        assert_eq!(out[0].byte_count, 5);
        assert_eq!(out[1].text, "second");
        assert_eq!(out[2].text, "third");
    }

    #[test]
    fn extract_reasoning_ignores_non_assistant_shapes() {
        let bodies = vec![
            jsonb(json!({"role": "user", "contents": [{"kind": "text", "value": "hi"}]})),
            jsonb(json!({"role": "assistant"})), // missing `contents`
            jsonb(json!({"role": "assistant", "contents": "not an array"})),
        ];
        let out = extract_reasoning(&bodies);
        assert!(out.is_empty());
    }

    #[test]
    fn extract_reasoning_respects_cap() {
        // One body carrying many blocks — the inner break must fire.
        let blocks: Vec<JsonValue> = (0..MAX_REASONING_BLOCKS_PER_TURN + 10)
            .map(|i| json!({"kind": "reasoning", "value": format!("{i}")}))
            .collect();
        let bodies = vec![jsonb(json!({
            "role": "assistant",
            "contents": blocks,
        }))];
        let out = extract_reasoning(&bodies);
        assert_eq!(out.len(), MAX_REASONING_BLOCKS_PER_TURN);
    }
}
