//! Chat-thread endpoints used by the channel-feed UI.
//!
//! - `GET /threads`                       — channel feed (G1)
//! - `GET /threads/{root}/messages`       — flat thread history (G2)
//! - `GET /threads/{root}/stream`         — live DAG-wide SSE (G3)
//!
//! See `doc/backend_plan.md` for the full design.

use std::convert::Infallible;
use std::time::Duration;

use axum::Json;
use axum::Router;
use axum::extract::{Path, Query, State};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::routing::get;
use chrono::{DateTime, Utc};
use futures::stream::{Stream, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tracing::warn;
use uuid::Uuid;

use crate::agents::AgentId;
use crate::auth::{AuthError, Principal};
use crate::runtime::{
    DEFAULT_THREAD_HISTORY_LIMIT, DEFAULT_THREAD_LIST_LIMIT, MAX_THREAD_HISTORY_LIMIT,
    MAX_THREAD_LIST_LIMIT, PromptRequestId, RequestStatus, ResponseChunk, THREAD_PREVIEW_MAX_CHARS,
    ThreadStreamEvent, ThreadStreamItem,
};
use crate::session::SessionId;
use crate::types::{MessageSender, MessageSenderKind, Participant, ParticipantKind};

use super::super::error::HttpError;
use super::super::state::AppState;

const SSE_KEEPALIVE_INTERVAL: Duration = Duration::from_secs(15);

pub(super) fn router() -> Router<AppState> {
    Router::new()
        .route("/threads", get(list_threads))
        .route("/threads/{id}/messages", get(thread_messages))
        .route("/threads/{id}/stream", get(stream_thread))
}

// ─── G1 ─────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct ListThreadsQuery {
    #[serde(default)]
    before: Option<DateTime<Utc>>,
    #[serde(default)]
    limit: Option<u32>,
}

#[derive(Debug, Serialize)]
struct ThreadAgentRef {
    id: AgentId,
    name: String,
}

#[derive(Debug, Serialize)]
struct ThreadSummary {
    root_request_id: PromptRequestId,
    root_session_id: SessionId,
    first_agent: ThreadAgentRef,
    preview: String,
    reply_count: i64,
    last_activity_at: DateTime<Utc>,
    status: RequestStatus,
    created_at: DateTime<Utc>,
}

type ThreadRow = (
    PromptRequestId,
    SessionId,
    AgentId,
    String,
    String,
    i64,
    DateTime<Utc>,
    RequestStatus,
    DateTime<Utc>,
);

/// Channel-feed query. The cursor (`$2`) is optional — `NULL` skips the
/// `last_activity_at < $2` predicate, so one parameterised query covers
/// both the head and the cursor-bound page (CLAUDE.md §10 — still no
/// string concatenation, just optional binds).
///
/// `thread_stats` is scoped to the human-rooted DAGs by joining `sessions`
/// to `prompt_requests`, so the GROUP BY only walks sessions belonging to
/// rows we'd return anyway — keeps the aggregate from sweeping every
/// session in the database.
///
/// `reply_count` mirrors the FE's bubble fold (`web/src/lib/chatBody.ts` —
/// `foldHistoryIntoBubbles`): one bubble per delivered `send_message` call.
/// Plain assistant rows, system rows carrying tool_results, and the
/// human's own prompt are conversation plumbing, not user-visible replies.
// Filtering joins `prompt_requests` through `sessions`. Under
// `begin_as`, `sessions` is RLS-bound to the caller's org so only
// human roots whose conversation session belongs to the caller's org
// pass the inner JOIN — RLS provides isolation without the SQL
// having to name `org_id` itself.
const THREAD_LIST_SQL: &str = "WITH visible_human_roots AS (
    SELECT pr.id AS root_request_id, pr.session_id
    FROM prompt_requests pr
    JOIN sessions s ON s.id = pr.session_id
    WHERE pr.id = pr.root_request_id
      AND pr.sender_kind = 'human'
),
thread_stats AS (
    SELECT s.root_request_id,
           COUNT(*) FILTER (
               WHERE sm.sender_kind = 'agent'
                 AND sm.body @? '$.contents[*] ? (@.kind == \"tool_call\" && @.value.name == \"send_message\")'
           ) AS reply_count,
           MAX(sm.created_at) AS last_msg_at
    FROM sessions s
    JOIN visible_human_roots hr ON hr.root_request_id = s.root_request_id
    LEFT JOIN session_messages sm ON sm.session_id = s.id
    GROUP BY s.root_request_id
)
SELECT
    pr.id,
    pr.session_id,
    a.id,
    a.name,
    LEFT(pr.content, $1)        AS preview,
    COALESCE(ts.reply_count, 0) AS reply_count,
    GREATEST(pr.created_at,
             COALESCE(ts.last_msg_at, pr.created_at)) AS last_activity_at,
    pr.status,
    pr.created_at
FROM prompt_requests pr
JOIN visible_human_roots vhr ON vhr.root_request_id = pr.id
JOIN agents a ON a.id = pr.receiver_agent_id
LEFT JOIN thread_stats ts ON ts.root_request_id = pr.id
WHERE ($2::timestamptz IS NULL
       OR GREATEST(pr.created_at,
                   COALESCE(ts.last_msg_at, pr.created_at)) < $2)
ORDER BY last_activity_at DESC
LIMIT $3";

#[tracing::instrument(skip_all, name = "thread.list", fields(patom.thread.list.size = tracing::field::Empty))]
async fn list_threads(
    State(state): State<AppState>,
    principal: Principal,
    Query(q): Query<ListThreadsQuery>,
) -> Result<Json<Vec<ThreadSummary>>, HttpError> {
    let limit = q
        .limit
        .unwrap_or(DEFAULT_THREAD_LIST_LIMIT)
        .clamp(1, MAX_THREAD_LIST_LIMIT);
    let preview_chars = i32::try_from(THREAD_PREVIEW_MAX_CHARS)
        .expect("invariant: THREAD_PREVIEW_MAX_CHARS fits in i32");

    // Tenant-scoped read: the `thread_stats` CTE joins through
    // `sessions`, which now has an `org_id` column and an isolation
    // policy. `begin_as` sets `app.user_id` so the policy filters to
    // the caller's org. The `prompt_requests` retrofit lands later;
    // until then the outer SELECT against `prompt_requests` is
    // unfiltered, but the inner JOIN to RLS-bound `sessions` keeps the
    // wire response tenant-scoped to roots whose conversation session
    // the caller can see.
    let mut tx = crate::auth::begin_as(&state.pool, &principal).await?;
    let rows: Vec<ThreadRow> = sqlx::query_as(THREAD_LIST_SQL)
        .bind(preview_chars)
        .bind(q.before)
        .bind(i64::from(limit))
        .fetch_all(&mut *tx)
        .await
        .map_err(AuthError::from)?;
    tx.commit().await.map_err(AuthError::from)?;

    tracing::Span::current().record("patom.thread.list.size", rows.len());
    Ok(Json(rows.into_iter().map(thread_row_to_summary).collect()))
}

fn thread_row_to_summary(row: ThreadRow) -> ThreadSummary {
    let (
        root_request_id,
        root_session_id,
        agent_id,
        agent_name,
        preview,
        reply_count,
        last_activity_at,
        status,
        created_at,
    ) = row;
    ThreadSummary {
        root_request_id,
        root_session_id,
        first_agent: ThreadAgentRef {
            id: agent_id,
            name: agent_name,
        },
        preview,
        reply_count,
        last_activity_at,
        status,
        created_at,
    }
}

// ─── G2 ─────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct ThreadMessagesQuery {
    #[serde(default)]
    before_ts: Option<DateTime<Utc>>,
    #[serde(default)]
    before_seq: Option<i64>,
    #[serde(default)]
    limit: Option<u32>,
}

#[derive(Debug, Serialize)]
struct ThreadMessage {
    session_id: SessionId,
    seq: i64,
    sender: MessageSender,
    receiver: Participant,
    body: serde_json::Value,
    created_at: DateTime<Utc>,
    /// The prompt request that produced this row. Surfaced on the wire so
    /// the FE can dedupe optimistic / live / persisted bubbles by identity.
    request_id: PromptRequestId,
}

type HistoryRow = (
    SessionId,
    i64,
    MessageSenderKind,
    Option<AgentId>,
    ParticipantKind,
    Option<AgentId>,
    serde_json::Value,
    DateTime<Utc>,
    PromptRequestId,
);

/// Thread-history query. As with G1, the `(before_ts, before_seq)` cursor
/// is optional; passing both as `NULL` skips the lexicographic predicate.
const THREAD_HISTORY_SQL: &str = "SELECT sm.session_id, sm.seq,
        sm.sender_kind, sm.sender_agent_id,
        sm.receiver_kind, sm.receiver_agent_id,
        sm.body, sm.created_at, sm.request_id
 FROM session_messages sm
 JOIN sessions s ON s.id = sm.session_id
 WHERE s.root_request_id = $1
   AND ($2::timestamptz IS NULL
        OR (sm.created_at, sm.seq) < ($2, $3))
 ORDER BY sm.created_at, sm.seq
 LIMIT $4";

#[tracing::instrument(
    skip_all,
    name = "thread.history",
    fields(
        patom.dag.root = %root,
        patom.thread.history.size = tracing::field::Empty,
    ),
)]
async fn thread_messages(
    State(state): State<AppState>,
    principal: Principal,
    Path(root): Path<Uuid>,
    Query(q): Query<ThreadMessagesQuery>,
) -> Result<Json<Vec<ThreadMessage>>, HttpError> {
    let root = PromptRequestId::from(root);
    let limit = q
        .limit
        .unwrap_or(DEFAULT_THREAD_HISTORY_LIMIT)
        .clamp(1, MAX_THREAD_HISTORY_LIMIT);

    // Cursor: both fields go together — one without the other under-pins
    // the row and would silently drop a tied (created_at, seq) pair.
    let (before_ts, before_seq) = match (q.before_ts, q.before_seq) {
        (Some(ts), Some(seq)) => (Some(ts), seq),
        (None, None) => (None, 0),
        _ => {
            return Err(HttpError::BadRequest(
                "before_ts and before_seq must be supplied together".into(),
            ));
        }
    };

    // Tenant-scoped read: `session_messages` is RLS-bound on `org_id`
    // so the join through `sessions` filters to rows the caller can see.
    // A cross-org `root` id therefore yields an empty result — the same
    // shape as "root exists but has no messages yet" — rather than
    // leaking a 404 vs 200 distinction across orgs.
    let mut tx = crate::auth::begin_as(&state.pool, &principal).await?;
    let rows: Vec<HistoryRow> = sqlx::query_as(THREAD_HISTORY_SQL)
        .bind(root)
        .bind(before_ts)
        .bind(before_seq)
        .bind(i64::from(limit))
        .fetch_all(&mut *tx)
        .await
        .map_err(AuthError::from)?;
    tx.commit().await.map_err(AuthError::from)?;

    tracing::Span::current().record("patom.thread.history.size", rows.len());
    Ok(Json(rows.into_iter().map(history_row_to_message).collect()))
}

fn history_row_to_message(row: HistoryRow) -> ThreadMessage {
    let (session_id, seq, sk, said, rk, raid, body, created_at, request_id) = row;
    let sender = MessageSender::from_kind_id(sk, said)
        .expect("invariant: session_messages.sender_* shape enforced by CHECK");
    let receiver = Participant::from_kind_id(rk, raid)
        .expect("invariant: session_messages.receiver_* shape enforced by CHECK");
    ThreadMessage {
        session_id,
        seq,
        sender,
        receiver,
        body,
        created_at,
        request_id,
    }
}

// ─── G3 ─────────────────────────────────────────────────────────────────

async fn stream_thread(
    State(state): State<AppState>,
    principal: Principal,
    Path(root): Path<Uuid>,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, HttpError> {
    let root = PromptRequestId::from(root);
    // Authorisation gate: only subscribe if the caller can see *any*
    // session in this DAG. RLS on `sessions` filters cross-org rows
    // automatically — if the caller can't see them, the lookup returns
    // None and we 404 cleanly without subscribing to the broadcast.
    {
        let mut tx = crate::auth::begin_as(&state.pool, &principal).await?;
        let visible: Option<(SessionId,)> =
            sqlx::query_as("SELECT id FROM sessions WHERE root_request_id = $1 LIMIT 1")
                .bind(root)
                .fetch_optional(&mut *tx)
                .await
                .map_err(AuthError::from)?;
        tx.commit().await.map_err(AuthError::from)?;
        if visible.is_none() {
            return Err(HttpError::NotFound);
        }
    }
    let inner = state.thread_stream.subscribe(root);

    // Per-connection monotonic cursor for the SSE `id:` header. Lossy on
    // process restart by design (G3 in `doc/backend_plan.md`); FE refetches
    // G2 and dedupes by `(request_id, chunk_seq)`.
    let mut cursor: u64 = 0;

    // `scan` forwards the terminal chunk emitted on DAG quiescence (worker's
    // `maybe_emit_quiescence` publishes `Done` on the root) and ends the
    // stream on the next call, so the FE sees the close signal.
    let stream = inner
        .scan(false, |closed, res| {
            let stop = *closed;
            if matches!(&res, Ok(ThreadStreamEvent::Item(item)) if item.chunk.is_terminal()) {
                *closed = true;
            }
            std::future::ready(if stop { None } else { Some(res) })
        })
        .map(move |res| {
            let event = match res {
                Ok(ThreadStreamEvent::Item(item)) => item_to_sse(cursor, &item),
                Ok(ThreadStreamEvent::Stalled) => synthetic_to_sse(cursor, &ResponseChunk::Stalled),
                Err(e) => {
                    warn!(error = %e, "thread.stream.error");
                    synthetic_to_sse(
                        cursor,
                        &ResponseChunk::Error {
                            reason: e.to_string(),
                        },
                    )
                }
            };
            cursor = cursor.saturating_add(1);
            Ok::<_, Infallible>(event)
        });

    Ok(Sse::new(stream).keep_alive(KeepAlive::new().interval(SSE_KEEPALIVE_INTERVAL)))
}

fn item_to_sse(cursor: u64, item: &ThreadStreamItem) -> Event {
    sse_event(
        cursor,
        &json!({
            "request_id": item.request_id,
            "from_agent": item.from_agent,
            "chunk_seq":  item.chunk_seq.get(),
            "chunk":      &item.chunk,
        }),
        item.chunk.event_kind(),
    )
}

/// Synthetic stream event with no underlying request — `Stalled` from the
/// broadcast lag path, `Error` from a fan-in fault. The wire envelope is
/// the same shape so the FE has one parser.
fn synthetic_to_sse(cursor: u64, chunk: &ResponseChunk) -> Event {
    sse_event(
        cursor,
        &json!({
            "request_id": serde_json::Value::Null,
            "from_agent": serde_json::Value::Null,
            "chunk_seq":  serde_json::Value::Null,
            "chunk":      chunk,
        }),
        chunk.event_kind(),
    )
}

fn sse_event(cursor: u64, body: &serde_json::Value, kind: &'static str) -> Event {
    let body = serde_json::to_string(body).expect("invariant: thread stream envelope serializes");
    Event::default()
        .id(cursor.to_string())
        .event(kind)
        .data(body)
}
