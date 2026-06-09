//! Chat-thread endpoints used by the channel-feed UI.
//!
//! - `GET /threads`                          — channel / DM thread index (G1)
//! - `GET /threads/{thread_id}/messages`     — canonical flat feed (G2)
//! - `GET /threads/{thread_id}/stream`       — live SSE (G3)
//!
//! All three read the thread model via [`crate::threads::ThreadStore`]
//! (`list_threads` / `feed` / `visible_to`). G3 subscribes the per-thread
//! fan-in ([`crate::runtime::PgThreadStream`]) keyed by `thread_id`: every chunk
//! published on any `prompt_requests` row in the thread (across the many DAGs a
//! thread hosts) is forwarded. The stream is continuous — a `Done`/`Error`
//! chunk is a per-turn marker, not a close.

use std::collections::HashMap;
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
use crate::auth::{Caller, Principal, UserId, UserProfileLite};
use crate::channels::ChannelId;
use crate::runtime::{PromptRequestId, ResponseChunk, ThreadStreamEvent, ThreadStreamItem};
use crate::threads::{DEFAULT_THREAD_FEED, MAX_THREAD_FEED, PgThreadStore, ThreadId, ThreadStore};
use crate::types::{MessageSender, Participant};

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
    /// Feed selector. `Some(id)` returns that channel's threads (member-gated);
    /// omitted returns the caller's direct-message threads (`channel_id IS NULL`,
    /// created by the caller). Member-scoping + active-org pin live in
    /// [`ThreadStore::list_threads`].
    #[serde(default)]
    channel_id: Option<Uuid>,
}

/// One row of the channel / DM thread index (G1). The thread feed is read
/// separately via `GET /threads/{id}/messages` (G2); a thread is no longer
/// "rooted" on a single human↔agent pair, so the row carries just the thread
/// identity + its location + last activity. Preview / unread are FE follow-ups.
#[derive(Debug, Serialize)]
struct ThreadSummary {
    thread_id: ThreadId,
    channel_id: Option<ChannelId>,
    last_activity_at: DateTime<Utc>,
}

#[tracing::instrument(skip_all, name = "thread.list", fields(patom.thread.list.size = tracing::field::Empty))]
async fn list_threads(
    State(state): State<AppState>,
    principal: Principal,
    Query(q): Query<ListThreadsQuery>,
) -> Result<Json<Vec<ThreadSummary>>, HttpError> {
    // Member-scoped (not single-creator) + active-org-pinned; the store owns
    // the RLS-bound query (P7). A fresh `PgThreadStore` is a pair of `Arc`
    // clones — the route's `pool`/`clock` are the inline seam (§ AppState.pool).
    let caller = Caller::new(principal.user_id, principal.active_org_id);
    let channel = q.channel_id.map(ChannelId::from);
    let store = PgThreadStore::new(state.pool.clone(), state.clock.clone());
    let items = store
        .list_threads(&caller, channel)
        .await
        .map_err(thread_store_error)?;
    tracing::Span::current().record("patom.thread.list.size", items.len());
    let summaries = items
        .into_iter()
        .map(|i| ThreadSummary {
            thread_id: i.thread_id,
            channel_id: i.channel_id,
            last_activity_at: i.last_activity_at,
        })
        .collect();
    Ok(Json(summaries))
}

/// Look up a sender's resolved display name + avatar. `None` when the user
/// isn't in the batch — normally impossible (the `users` FK on the
/// colleague row guarantees a profile), so a miss means the row was deleted
/// between the RLS read and the enrichment. Callers degrade gracefully.
fn resolve_profile(
    profiles: &HashMap<UserId, UserProfileLite>,
    user_id: UserId,
) -> Option<(String, Option<String>)> {
    profiles
        .get(&user_id)
        .map(|p| (p.name.clone(), p.avatar_url.clone()))
}

/// Map a thread-store failure to an HTTP status. A read fault is internal —
/// an invisible / missing thread surfaces as an empty page from the store's
/// visibility gate, not an error, so there's no NotFound branch here.
fn thread_store_error(e: crate::threads::ThreadError) -> HttpError {
    tracing::error!(error = %e, "thread.store.error");
    HttpError::Internal
}

// ─── G2 ─────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct ThreadMessagesQuery {
    /// Keyset cursor: return rows with `seq < before_seq` (page backwards).
    /// Omitted ⇒ the most recent page.
    #[serde(default)]
    before_seq: Option<i64>,
    #[serde(default)]
    limit: Option<u32>,
}

/// One row of the canonical flat feed (G2). Unlike the legacy per-pair
/// history, this exposes `kind` (posted chat vs. an agent's private
/// reasoning / tool_use / … artifact, shown to all for transparency, §2),
/// carries `owner_agent_id` for artifact rows, and a `sender` that may be
/// any party ([`MessageSender::System`] on a system row) — not the viewer
/// stamped onto every row. The store decodes both sides; the route only
/// enriches the human display name.
#[derive(Debug, Serialize)]
struct ThreadMessage {
    seq: i64,
    kind: &'static str,
    sender: MessageSender,
    owner_agent_id: Option<AgentId>,
    receiver: Option<Participant>,
    body: serde_json::Value,
    created_at: DateTime<Utc>,
    /// The producing turn for agent rows; `None` for plain human posts.
    request_id: Option<PromptRequestId>,
    /// Resolved display name of a *human* sender, enriched from the
    /// privileged user store (the tenant tx can't read `users`). `None`
    /// for agent / system rows.
    sender_display_name: Option<String>,
    sender_avatar_url: Option<String>,
}

#[tracing::instrument(
    skip_all,
    name = "thread.history",
    fields(
        patom.thread.id = %thread,
        patom.thread.history.size = tracing::field::Empty,
    ),
)]
async fn thread_messages(
    State(state): State<AppState>,
    principal: Principal,
    Path(thread): Path<Uuid>,
    Query(q): Query<ThreadMessagesQuery>,
) -> Result<Json<Vec<ThreadMessage>>, HttpError> {
    let thread = ThreadId::from(thread);
    let limit = q.limit.unwrap_or(DEFAULT_THREAD_FEED);

    // The store runs the read RLS-scoped under the caller and gates thread
    // visibility (channel membership / DM ownership) + active-org pin, so a
    // thread the caller can't see yields an empty page rather than a leak.
    let caller = Caller::new(principal.user_id, principal.active_org_id);
    let store = PgThreadStore::new(state.pool.clone(), state.clock.clone());
    let rows = store
        .feed(&caller, thread, q.before_seq, limit)
        .await
        .map_err(thread_store_error)?;

    tracing::Span::current().record("patom.thread.history.size", rows.len());

    // Enrich human senders with name + avatar via the privileged store —
    // the RLS read can't touch `users` (migration 14). Bounded by the feed
    // LIMIT (CLAUDE.md §5).
    assert!(
        i64::try_from(rows.len()).unwrap_or(i64::MAX) <= MAX_THREAD_FEED,
        "invariant: feed LIMIT enforces MAX_THREAD_FEED ceiling"
    );
    let sender_ids: Vec<UserId> = rows.iter().filter_map(|m| m.sender.user_id()).collect();
    let profiles = state.users.read_profiles(&sender_ids).await?;

    let messages = rows
        .into_iter()
        .map(|m| feed_message_to_wire(m, &profiles))
        .collect();
    Ok(Json(messages))
}

/// Project one store [`FeedMessage`] onto the G2 wire shape. The store already
/// decoded both participant sides via the canonical parser, so the route only
/// enriches the human sender's display name/avatar from the privileged store.
fn feed_message_to_wire(
    m: crate::threads::FeedMessage,
    profiles: &HashMap<UserId, UserProfileLite>,
) -> ThreadMessage {
    let (sender_display_name, sender_avatar_url) = m.sender.user_id().map_or((None, None), |uid| {
        resolve_profile(profiles, uid).map_or((None, None), |(n, a)| (Some(n), a))
    });
    ThreadMessage {
        seq: m.seq,
        kind: m.kind.as_str(),
        sender: m.sender,
        owner_agent_id: m.owner_agent_id,
        receiver: m.receiver,
        body: m.body,
        created_at: m.created_at,
        request_id: m.request_id,
        sender_display_name,
        sender_avatar_url,
    }
}

// ─── G3 ─────────────────────────────────────────────────────────────────

async fn stream_thread(
    State(state): State<AppState>,
    principal: Principal,
    Path(id): Path<Uuid>,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, HttpError> {
    let thread = ThreadId::from(id);
    // Authorisation gate: only subscribe if the caller can see this thread in
    // their active org (channel membership, or DM ownership; active-org pin in
    // the store). RLS isolates by membership (any org), so a multi-org caller
    // must not stream a thread from a non-active org. An invisible / missing
    // thread returns `false` and we 404 cleanly without subscribing.
    let caller = Caller::new(principal.user_id, principal.active_org_id);
    let store = PgThreadStore::new(state.pool.clone(), state.clock.clone());
    if !store
        .visible_to(&caller, thread)
        .await
        .map_err(thread_store_error)?
    {
        return Err(HttpError::NotFound);
    }
    let inner = state.thread_stream.subscribe(thread);

    // Per-connection monotonic cursor for the SSE `id:` header. Lossy on
    // process restart by design (G3 in `doc/backend_plan.md`); FE refetches
    // G2 and dedupes by `(request_id, chunk_seq)`.
    let mut cursor: u64 = 0;

    // The thread feed is continuous — a `Done`/`Error` chunk is a per-turn
    // marker (one DAG of many the thread hosts), NOT a stream close. So we do
    // NOT close the SSE on a terminal chunk; we forward every event and the
    // connection lives until the client disconnects.
    let stream = inner.map(move |res| {
        let event = match res {
            Ok(ThreadStreamEvent::Item(item)) => item_to_sse(cursor, &item),
            Ok(ThreadStreamEvent::Stalled) => synthetic_to_sse(cursor, &ResponseChunk::Stalled),
            Err(e) => {
                warn!(error = %e, "thread.stream.error");
                synthetic_to_sse(
                    cursor,
                    &ResponseChunk::Error {
                        reason: e.to_string(),
                        code: "stream".to_owned(),
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
