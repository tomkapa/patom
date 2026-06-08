//! Fan-in subscriber for DAG-wide chat threads.
//!
//! The single-process companion to [`super::pg_response::PgResponseHub`].
//! Where the response hub keeps a per-request broadcast slot, this type
//! demuxes Postgres `LISTEN/NOTIFY` deliveries into per-thread (per-DAG-root)
//! broadcast slots. The HTTP handler at `GET /threads/{root}/stream`
//! subscribes a single broadcast receiver and forwards every chunk emitted on
//! any `prompt_requests` row in the DAG.
//!
//! ```text
//! pg_response.publish(req)  ──INSERT──►  prompt_response_chunks
//!                            └─NOTIFY──►  patom_thread_chunk    ┐
//!                                                                ▼
//!                                             PgThreadStream::run_loop
//!                                              ├─ fetch chunk by (req,seq)
//!                                              └─ broadcast(root)
//!                                                       │
//!                                          GET /threads/{root}/stream
//! ```
//!
//! Single LISTEN connection per process: the listener task drains
//! [`PgListener::recv`] in a loop, parses the notify payload, fetches the
//! chunk row from `prompt_response_chunks`, and broadcasts a
//! [`ThreadStreamEvent::Item`] to every subscriber on that root. A slow
//! subscriber that exhausts its broadcast buffer receives a
//! [`ThreadStreamEvent::Stalled`]; the SSE handler reports it back to the
//! client which reconnects and refetches `GET /threads/{id}/messages` to
//! dedupe.
//!
//! **No persistence guarantee.** The per-thread cursor is in-memory and lossy
//! on process restart — the design (see `doc/backend_plan.md` G3) is that
//! resume is best-effort and full correctness comes from the client
//! refetching G2 and deduping by `(request_id, chunk_seq)`. The chunk *log*
//! still lives durably in `prompt_response_chunks`.

use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::Mutex;

use futures::Stream;
use serde::Deserialize;
use sqlx::PgPool;
use sqlx::postgres::PgListener;
use thiserror::Error;
use tokio::sync::broadcast;
use tokio_stream::StreamExt as _;
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::wrappers::errors::BroadcastStreamRecvError;
use tokio_util::sync::{CancellationToken, DropGuard};
use tracing::{debug, info, warn};

use crate::agents::AgentId;
use crate::session::SessionId;

use super::limits::{MAX_THREAD_CHUNK_BUFFER, MAX_THREAD_SLOTS, THREAD_NOTIFY_CHANNEL};
use super::response::ResponseChunk;
use super::types::{ChunkSeq, PromptRequestId};

/// Backend / parsing errors observable on the thread-stream fan-in path.
#[derive(Debug, Error)]
pub enum ThreadStreamError {
    #[error("thread stream backend: {0}")]
    Backend(String),

    #[error("thread stream db: {0}")]
    Db(#[from] sqlx::Error),
}

/// One fan-in chunk on a thread's stream.
#[derive(Debug, Clone)]
pub struct ThreadStreamItem {
    pub request_id: PromptRequestId,
    /// Session the chunk belongs to. Identical to `prompt_requests.session_id`
    /// — surfaced here so delivery surfaces (e.g. the Slack stream pump) can
    /// route a chunk to the right `(agent, human)` surface without an extra
    /// round-trip.
    pub session_id: SessionId,
    /// Authoring agent. For [`ResponseChunk::AgentMessage`] this is the
    /// chunk's own `from` (a deeper-DAG agent addressing the human); for
    /// every other chunk it is the request's `receiver_agent_id` — i.e. the
    /// agent whose turn produced it.
    pub from_agent: AgentId,
    pub chunk_seq: ChunkSeq,
    pub chunk: ResponseChunk,
}

/// What a thread-stream subscriber observes. The `Stalled` arm comes out of
/// the broadcast lag path — same semantics as the per-request hub: reconnect
/// + refetch backlog via `GET /threads/{id}/messages`.
#[derive(Debug, Clone)]
pub enum ThreadStreamEvent {
    Item(ThreadStreamItem),
    Stalled,
}

/// Wire shape of the JSON payload published by [`super::pg_response::PgResponseHub`]
/// on `LISTEN patom_thread_chunk`. Built from the `prompt_requests` row at
/// publish time so the listener can route by `root_request_id` without an
/// extra query. `chunk_seq` rides as a JSON number (Postgres BIGINT
/// values up to 2^53 are safe in JSON; chunk sequences never approach that
/// in practice).
#[derive(Debug, Deserialize)]
struct NotifyPayload {
    request_id: PromptRequestId,
    root_request_id: PromptRequestId,
    session_id: SessionId,
    chunk_seq: u64,
}

impl NotifyPayload {
    fn chunk_seq(&self) -> ChunkSeq {
        ChunkSeq::from(self.chunk_seq)
    }
}

/// One per-root broadcast slot. Created lazily (on first subscribe or first
/// notify routed to this root) and evicted under cap.
#[derive(Debug)]
struct ThreadSlot {
    tx: broadcast::Sender<ThreadStreamEvent>,
}

impl ThreadSlot {
    fn new() -> Self {
        let (tx, _rx) = broadcast::channel(MAX_THREAD_CHUNK_BUFFER);
        Self { tx }
    }
}

#[derive(Debug)]
struct SlotTable {
    slots: HashMap<PromptRequestId, ThreadSlot>,
    /// Insertion order; drives eviction (no-receivers-first, oldest fallback).
    order: VecDeque<PromptRequestId>,
}

impl SlotTable {
    fn empty() -> Self {
        Self {
            slots: HashMap::new(),
            order: VecDeque::new(),
        }
    }
}

/// Touch the slot for `root`, creating it if needed. On creation, if the
/// table is at `slot_cap` we evict the oldest slot whose broadcast has no
/// live receivers, falling back to the literal oldest entry.
fn touch_slot(table: &mut SlotTable, root: PromptRequestId, slot_cap: usize) -> &mut ThreadSlot {
    if !table.slots.contains_key(&root) {
        if table.slots.len() >= slot_cap {
            evict_one(table);
        }
        table.slots.insert(root, ThreadSlot::new());
        table.order.push_back(root);
    }
    table
        .slots
        .get_mut(&root)
        .expect("invariant: slot was just inserted or already present")
}

fn evict_one(table: &mut SlotTable) {
    let stale_idx = table.order.iter().position(|id| {
        table
            .slots
            .get(id)
            .is_some_and(|s| s.tx.receiver_count() == 0)
    });
    let victim = if let Some(idx) = stale_idx {
        table.order.remove(idx)
    } else {
        warn!(
            slots = table.slots.len(),
            "thread.stream.evict_live: slot cap reached with no idle slots; dropping oldest"
        );
        table.order.pop_front()
    };
    if let Some(id) = victim {
        table.slots.remove(&id);
    }
}

/// Cheap-clone handle to the fan-in subscriber. Held by HTTP routes via
/// [`crate::http::AppState`].
pub type SharedThreadStream = Arc<PgThreadStream>;

/// Postgres-backed DAG fan-in stream. Owns one [`PgListener`] connection,
/// demuxes notifications into per-thread broadcasts, and exposes
/// [`Self::subscribe`] to HTTP handlers.
pub struct PgThreadStream {
    table: Arc<Mutex<SlotTable>>,
    slot_cap: usize,
    /// Drop fires the cancellation token, terminating the listener task and
    /// detaching the spawned `JoinHandle`. Single shutdown contract.
    _guard: DropGuard,
}

impl PgThreadStream {
    /// Build the stream and spawn the listener task. The task dies when
    /// `cancel` fires or when this struct (and its internal [`DropGuard`])
    /// is dropped — whichever comes first.
    pub async fn spawn(pool: PgPool, cancel: CancellationToken) -> Result<Arc<Self>, sqlx::Error> {
        Self::spawn_with_caps(pool, cancel, MAX_THREAD_SLOTS).await
    }

    pub async fn spawn_with_caps(
        pool: PgPool,
        cancel: CancellationToken,
        slot_cap: usize,
    ) -> Result<Arc<Self>, sqlx::Error> {
        assert!(slot_cap > 0, "invariant: slot_cap must be > 0");
        let mut listener = PgListener::connect_with(&pool).await?;
        listener.listen(THREAD_NOTIFY_CHANNEL).await?;

        let table = Arc::new(Mutex::new(SlotTable::empty()));
        let listener_table = Arc::clone(&table);
        let listener_cancel = cancel.clone();
        tokio::spawn(async move {
            run_listener(listener, pool, listener_table, listener_cancel).await;
        });

        Ok(Arc::new(Self {
            table,
            slot_cap,
            _guard: cancel.drop_guard(),
        }))
    }

    fn table(&self) -> std::sync::MutexGuard<'_, SlotTable> {
        self.table
            .lock()
            .expect("invariant: thread-stream mutex never poisoned")
    }

    /// Subscribe to live fan-in for `root`. The returned stream attaches to
    /// the per-thread broadcast and yields one event per chunk emitted on
    /// any request whose `root_request_id == root`. Backlog replay is *not*
    /// performed here — the FE refetches `GET /threads/{root}/messages`
    /// when (re)opening a thread, then dedupes by `(request_id, chunk_seq)`.
    /// Lossy on process restart by design (G3 in `doc/backend_plan.md`).
    #[tracing::instrument(
        skip(self),
        name = "thread.stream.subscribe",
        fields(patom.dag.root = %root),
    )]
    pub fn subscribe(&self, root: PromptRequestId) -> ThreadStream {
        let rx = {
            let mut guard = self.table();
            let slot = touch_slot(&mut guard, root, self.slot_cap);
            slot.tx.subscribe()
        };
        let mapped = BroadcastStream::new(rx).map(|res| match res {
            Ok(evt) => Ok(evt),
            Err(BroadcastStreamRecvError::Lagged(_)) => Ok(ThreadStreamEvent::Stalled),
        });
        Box::pin(mapped)
    }
}

impl fmt::Debug for PgThreadStream {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PgThreadStream")
            .field("slot_cap", &self.slot_cap)
            .finish_non_exhaustive()
    }
}

/// Boxed stream of fan-in events. Same shape as
/// [`super::response::RequestStream`] modulo item type.
pub type ThreadStream =
    Pin<Box<dyn Stream<Item = Result<ThreadStreamEvent, ThreadStreamError>> + Send>>;

/// Listener task body. Lives until `cancel` fires or the
/// [`PgListener::recv`] connection drops. Each notification is parsed,
/// the matching chunk is fetched from `prompt_response_chunks`, and the
/// per-root broadcast slot receives a [`ThreadStreamEvent::Item`].
///
/// Errors during parse / fetch are logged at WARN and skip the chunk —
/// the SSE client will refetch G2 if it cares. We never poison the loop on
/// a single bad notification.
#[tracing::instrument(
    skip_all,
    name = "thread.notify.fan_in",
    fields(patom.thread.notify.channel = THREAD_NOTIFY_CHANNEL),
)]
async fn run_listener(
    mut listener: PgListener,
    pool: PgPool,
    table: Arc<Mutex<SlotTable>>,
    cancel: CancellationToken,
) {
    info!("thread.notify.listener.started");
    loop {
        tokio::select! {
            () = cancel.cancelled() => {
                info!("thread.notify.listener.shutdown");
                return;
            }
            res = listener.recv() => {
                match res {
                    Ok(notify) => {
                        handle_notification(&pool, &table, notify.payload()).await;
                    }
                    Err(e) => {
                        // sqlx auto-reconnects when `eager_reconnect=true`
                        // (the default): a transient transport error becomes
                        // an `Err` here followed by future `recv` calls
                        // resubscribing. Log and keep looping.
                        warn!(error = %e, "thread.notify.listener.recv_error");
                    }
                }
            }
        }
    }
}

async fn handle_notification(pool: &PgPool, table: &Arc<Mutex<SlotTable>>, raw: &str) {
    let payload: NotifyPayload = match serde_json::from_str(raw) {
        Ok(p) => p,
        Err(e) => {
            warn!(error = %e, payload.len = raw.len(), "thread.notify.parse_error");
            return;
        }
    };
    let root = payload.root_request_id;

    // Skip the DB roundtrip when no subscriber is listening: the broadcast
    // would silently drop the event anyway, and we don't want to evict a
    // live slot just to populate one nobody reads. Clone the Sender under
    // the lock so we can release it before the await-suspending fetch.
    let sender = {
        let guard = table
            .lock()
            .expect("invariant: thread-stream mutex never poisoned");
        guard
            .slots
            .get(&root)
            .filter(|s| s.tx.receiver_count() > 0)
            .map(|s| s.tx.clone())
    };
    let Some(sender) = sender else {
        debug!(
            patom.dag.root = %root,
            patom.request.id = %payload.request_id,
            "thread.notify.dropped_no_subscribers",
        );
        return;
    };

    let item = match fetch_item(pool, &payload).await {
        Ok(item) => item,
        Err(e) => {
            warn!(error = %e, "thread.notify.fetch_error");
            return;
        }
    };

    // Send returns Err only when the last receiver dropped between the
    // clone above and here — benign.
    let _ = sender.send(ThreadStreamEvent::Item(item));
}

/// Fetch the chunk + receiver agent id for a (`request_id`, `chunk_seq`)
/// tuple. Single round-trip — both columns come from the join.
async fn fetch_item(
    pool: &PgPool,
    payload: &NotifyPayload,
) -> Result<ThreadStreamItem, ThreadStreamError> {
    let chunk_seq = payload.chunk_seq();
    // Privileged: the listener task is process-global infrastructure
    // — one Postgres connection demuxing notifications across every
    // tenant. RLS would otherwise filter to the (unset) anonymous
    // role's org. HTTP-side authorization happens at the
    // `GET /threads/{root}/stream` gate which runs `begin_as`
    // before subscribing.
    let mut tx = crate::auth::begin_privileged(pool).await?;
    // `receiver_agent_id` is joined out of the receiver colleague — the
    // prompt_requests table only stores `receiver_colleague_id` after
    // migration 60.
    let row: Option<(serde_json::Value, AgentId)> = sqlx::query_as(
        "SELECT prc.payload, rc.agent_id
         FROM prompt_response_chunks prc
         JOIN prompt_requests pr ON pr.id = prc.request_id
         JOIN colleagues rc ON rc.id = pr.receiver_colleague_id
         WHERE prc.request_id = $1 AND prc.seq = $2",
    )
    .bind(payload.request_id)
    .bind(chunk_seq)
    .fetch_optional(&mut *tx)
    .await?;
    tx.commit().await?;

    let (raw, receiver_agent_id) = row.ok_or_else(|| {
        ThreadStreamError::Backend(format!(
            "chunk row missing for request {} seq {chunk_seq}",
            payload.request_id
        ))
    })?;
    let chunk: ResponseChunk = serde_json::from_value(raw)
        .map_err(|e| ThreadStreamError::Backend(format!("deserialize chunk: {e}")))?;

    // AgentMessage carries its own author (a deeper-DAG agent addressing
    // the human via send_message); every other chunk is authored by the
    // request's receiver agent.
    let from_agent = match &chunk {
        ResponseChunk::AgentMessage { from, .. } => *from,
        _ => receiver_agent_id,
    };

    Ok(ThreadStreamItem {
        request_id: payload.request_id,
        session_id: payload.session_id,
        from_agent,
        chunk_seq,
        chunk,
    })
}
