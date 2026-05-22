//! Postgres-backed [`ResponseSink`] / [`ResponseSource`].
//!
//! Each chunk is persisted to `prompt_response_chunks` and (if `is_terminal`) flips
//! `prompt_response_streams.closed` to TRUE. The live tap uses an in-process
//! `tokio::sync::broadcast::Sender` per request — single-process today, so SSE
//! handlers and worker tasks share the same hub. When task1.md's "binary split"
//! lands, this is where LISTEN/NOTIFY plugs in; the trait surface does not change.
//!
//! `subscribe(id, since)` subscribes to the broadcast first, then reads any backlog
//! from the chunks table. The combined stream yields the backlog then live items
//! with `seq > backlog_max` so no chunk is observed twice (a publish that lands
//! between subscribe and SELECT shows up in both, and the seq filter dedupes it).
//!
//! Wall-clock timestamps come from the injected [`SharedClock`] (CLAUDE.md §11).

use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::sync::Mutex;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use futures::StreamExt;
use sqlx::PgPool;
use tokio::sync::broadcast;
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::wrappers::errors::BroadcastStreamRecvError;
use tracing::warn;

use crate::auth::{run_as_user, run_privileged};
use crate::clock::SharedClock;

use super::error::ResponseError;
use super::limits::{MAX_CHUNK_BUFFER_PER_REQUEST, THREAD_NOTIFY_CHANNEL};
use super::response::{
    RequestStream, ResponseChunk, ResponseChunkEnvelope, ResponseSink, ResponseSource, StreamEvent,
};
use super::types::{ChunkSeq, PromptRequestId};

/// Default cap on the in-process broadcast slot table.
///
/// Bounds memory growth across many distinct requests in a long-lived process; the
/// chunk *log* lives in Postgres, so this is purely the live-tap directory size.
/// Eviction prefers oldest closed slots (no live subscribers anyway), falling back
/// to oldest live with a warning when the cap is hit by sustained load.
pub const MAX_RESPONSE_SLOTS: usize = 4096;

#[derive(Debug)]
struct StreamSlot {
    tx: broadcast::Sender<ResponseChunkEnvelope>,
    closed: bool,
}

impl StreamSlot {
    fn new() -> Self {
        let (tx, _rx) = broadcast::channel(MAX_CHUNK_BUFFER_PER_REQUEST);
        Self { tx, closed: false }
    }
}

#[derive(Debug)]
struct SlotTable {
    slots: HashMap<PromptRequestId, StreamSlot>,
    /// Insertion order. Eviction walks from the front and prefers the oldest
    /// `closed == true` slot, falling back to the oldest entry when none are closed.
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

/// Postgres-backed publish/subscribe hub.
pub struct PgResponseHub {
    pool: PgPool,
    clock: SharedClock,
    table: Mutex<SlotTable>,
    slot_cap: usize,
}

impl PgResponseHub {
    #[must_use]
    pub fn new(pool: PgPool, clock: SharedClock) -> Self {
        Self::with_caps(pool, clock, MAX_RESPONSE_SLOTS)
    }

    #[must_use]
    pub fn with_caps(pool: PgPool, clock: SharedClock, slot_cap: usize) -> Self {
        assert!(slot_cap > 0, "invariant: slot_cap must be > 0");
        Self {
            pool,
            clock,
            table: Mutex::new(SlotTable::empty()),
            slot_cap,
        }
    }

    fn now(&self) -> DateTime<Utc> {
        self.clock.now_utc()
    }

    fn table(&self) -> std::sync::MutexGuard<'_, SlotTable> {
        self.table
            .lock()
            .expect("invariant: response hub mutex never poisoned")
    }

    /// Subscribe to the live broadcast for `id`, creating the slot if missing.
    /// Returns `(receiver, was_already_closed)`. Held only across a sync critical
    /// section so a `std::sync::Mutex` is correct here per tokio's own guidance.
    fn subscribe_live(
        &self,
        id: PromptRequestId,
    ) -> (broadcast::Receiver<ResponseChunkEnvelope>, bool) {
        let mut guard = self.table();
        let slot_cap = self.slot_cap;
        let slot = touch_slot(&mut guard, id, slot_cap);
        (slot.tx.subscribe(), slot.closed)
    }

    /// Get or create the publish-side sender for `id`. Updates the closed flag if
    /// `terminal` is true. Returns the sender plus whether the slot was just
    /// observed as already-closed (publish into a closed slot is a backend bug).
    fn publish_sender(
        &self,
        id: PromptRequestId,
        terminal: bool,
    ) -> Result<broadcast::Sender<ResponseChunkEnvelope>, ResponseError> {
        let mut guard = self.table();
        let slot_cap = self.slot_cap;
        let slot = touch_slot(&mut guard, id, slot_cap);
        if slot.closed {
            return Err(ResponseError::Backend(format!(
                "request {id} stream already closed"
            )));
        }
        if terminal {
            slot.closed = true;
        }
        Ok(slot.tx.clone())
    }

    fn mark_closed(&self, id: PromptRequestId) {
        let mut guard = self.table();
        let slot_cap = self.slot_cap;
        let slot = touch_slot(&mut guard, id, slot_cap);
        slot.closed = true;
    }
}

impl fmt::Debug for PgResponseHub {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PgResponseHub")
            .field("slot_cap", &self.slot_cap)
            .finish_non_exhaustive()
    }
}

/// Touch the slot for `id`, creating it if needed. On creation, if the table is at
/// `slot_cap` we evict — preferring the oldest closed slot, falling back to the oldest
/// entry when none is closed.
fn touch_slot(table: &mut SlotTable, id: PromptRequestId, slot_cap: usize) -> &mut StreamSlot {
    if !table.slots.contains_key(&id) {
        if table.slots.len() >= slot_cap {
            evict_one(table);
        }
        table.slots.insert(id, StreamSlot::new());
        table.order.push_back(id);
    }
    table
        .slots
        .get_mut(&id)
        .expect("invariant: slot was just inserted or already present")
}

fn evict_one(table: &mut SlotTable) {
    let closed_idx = table
        .order
        .iter()
        .position(|id| table.slots.get(id).is_some_and(|s| s.closed));
    let victim = if let Some(idx) = closed_idx {
        table.order.remove(idx)
    } else {
        warn!(
            slots = table.slots.len(),
            "response.hub.evict_live: slot cap reached with no closed slots; dropping oldest live"
        );
        table.order.pop_front()
    };
    if let Some(id) = victim {
        table.slots.remove(&id);
    }
}

#[async_trait]
impl ResponseSink for PgResponseHub {
    async fn publish(
        &self,
        request_id: PromptRequestId,
        chunk: ResponseChunk,
    ) -> Result<ChunkSeq, ResponseError> {
        let persisted = run_privileged(&self.pool, async |tx| {
            publish_in_tx(self, tx.tx_mut(), request_id, chunk).await
        })
        .await?;
        self.fanout_after_commit(request_id, persisted)
    }

    async fn publish_for_user(
        &self,
        acting_user_id: crate::auth::UserId,
        request_id: PromptRequestId,
        chunk: ResponseChunk,
    ) -> Result<ChunkSeq, ResponseError> {
        let persisted = run_as_user(&self.pool, acting_user_id, async |tx| {
            publish_in_tx(self, tx.tx_mut(), request_id, chunk).await
        })
        .await?;
        self.fanout_after_commit(request_id, persisted)
    }

    async fn close(&self, request_id: PromptRequestId) -> Result<(), ResponseError> {
        run_privileged(&self.pool, async |tx| {
            close_in_tx(tx.tx_mut(), request_id).await
        })
        .await?;
        self.mark_closed(request_id);
        Ok(())
    }

    async fn close_for_user(
        &self,
        acting_user_id: crate::auth::UserId,
        request_id: PromptRequestId,
    ) -> Result<(), ResponseError> {
        run_as_user(&self.pool, acting_user_id, async |tx| {
            close_in_tx(tx.tx_mut(), request_id).await
        })
        .await?;
        self.mark_closed(request_id);
        Ok(())
    }
}

/// Side effects produced inside the publish tx and fanned out *after*
/// the runner commits. Subscribers must never observe a chunk whose
/// row isn't yet visible.
struct PersistedChunk {
    seq: ChunkSeq,
    envelope: ResponseChunkEnvelope,
    terminal: bool,
}

impl PgResponseHub {
    fn fanout_after_commit(
        &self,
        request_id: PromptRequestId,
        persisted: PersistedChunk,
    ) -> Result<ChunkSeq, ResponseError> {
        let sender = self.publish_sender(request_id, persisted.terminal)?;
        // `send` returns Err only when there are no receivers — fine,
        // late subscribers replay from the chunks table.
        let _ = sender.send(persisted.envelope);
        Ok(persisted.seq)
    }
}

/// One round-trip: read parent's `(org_id, root_request_id)`, bump the
/// per-request seq counter (the streams upsert also pins the `closed`
/// flag), insert the chunk row denormalised on the same `org_id`, and
/// `pg_notify` the DAG thread stream — all as one CTE chain. The first
/// publish lands at seq 0 (post-bump `next_seq` is 1, so `next_seq - 1`
/// matches `ChunkSeq::ZERO` and the SSE `Last-Event-ID` contract). If
/// `req` resolves to zero rows (synthetic test id), the chain yields
/// zero rows and the caller surfaces the original error.
const PUBLISH_CTE_SQL: &str = "WITH req AS (
     SELECT id AS request_id, org_id, root_request_id, session_id
     FROM prompt_requests WHERE id = $1
 ), bumped AS (
     INSERT INTO prompt_response_streams (request_id, org_id, next_seq, closed)
     SELECT req.request_id, req.org_id, 1, $2 FROM req
     ON CONFLICT (request_id) DO UPDATE
         SET next_seq = prompt_response_streams.next_seq + 1,
             closed   = prompt_response_streams.closed OR EXCLUDED.closed
     RETURNING next_seq
 ), chunk_ins AS (
     INSERT INTO prompt_response_chunks
         (request_id, org_id, seq, payload, bytes, is_terminal, created_at)
     SELECT req.request_id, req.org_id, bumped.next_seq - 1, $3, $4, $2, $5
     FROM req, bumped
     RETURNING seq
 ), notify AS (
     SELECT pg_notify($6, json_build_object(
         'request_id', req.request_id,
         'root_request_id', req.root_request_id,
         'session_id', req.session_id,
         'chunk_seq', chunk_ins.seq
     )::text) AS _n
     FROM req, chunk_ins
 )
 SELECT bumped.next_seq FROM bumped, notify";

async fn publish_in_tx(
    hub: &PgResponseHub,
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    request_id: PromptRequestId,
    chunk: ResponseChunk,
) -> Result<PersistedChunk, ResponseError> {
    let now = hub.now();
    let terminal = chunk.is_terminal();
    let payload = serde_json::to_value(&chunk)
        .map_err(|e| ResponseError::Backend(format!("serialize chunk: {e}")))?;
    let weight_capped = chunk
        .weight()
        .min(usize::try_from(i32::MAX).expect("invariant: i32::MAX fits in usize"));
    let bytes = i32::try_from(weight_capped).expect("invariant: weight clamped to i32 range");

    let row: Option<(ChunkSeq,)> = sqlx::query_as(PUBLISH_CTE_SQL)
        .bind(request_id)
        .bind(terminal)
        .bind(&payload)
        .bind(bytes)
        .bind(now)
        .bind(THREAD_NOTIFY_CHANNEL)
        .fetch_optional(&mut **tx)
        .await?;
    let (next_seq,) = row.ok_or_else(|| {
        ResponseError::Backend(format!(
            "publish: request {request_id} has no prompt_requests row"
        ))
    })?;
    let seq = next_seq
        .get()
        .checked_sub(1)
        .map(ChunkSeq::from)
        .expect("invariant: post-bump next_seq is at least 1");

    Ok(PersistedChunk {
        seq,
        envelope: ResponseChunkEnvelope { seq, chunk },
        terminal,
    })
}

/// One round-trip: read `org_id` from the parent request and upsert
/// the closed flag in a single CTE. If `req` resolves to zero rows
/// (synthetic test id) the INSERT does nothing — benign, matches
/// the prior explicit None branch. The hub-side `mark_closed` runs
/// after the runner commits.
async fn close_in_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    request_id: PromptRequestId,
) -> Result<(), ResponseError> {
    sqlx::query(
        "WITH req AS (SELECT id, org_id FROM prompt_requests WHERE id = $1)
         INSERT INTO prompt_response_streams (request_id, org_id, next_seq, closed)
         SELECT req.id, req.org_id, 0, TRUE FROM req
         ON CONFLICT (request_id) DO UPDATE SET closed = TRUE",
    )
    .bind(request_id)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

#[async_trait]
impl ResponseSource for PgResponseHub {
    async fn subscribe(
        &self,
        request_id: PromptRequestId,
        since: Option<ChunkSeq>,
    ) -> Result<RequestStream, ResponseError> {
        // Subscribe to the live broadcast FIRST so any publish that lands between
        // this point and the backlog read is captured by both the broadcast and the
        // DB read; the seq-filter on the live stream then dedupes the overlap.
        let (live_rx, _was_closed) = self.subscribe_live(request_id);

        let (backlog_envelopes, backlog_max) = read_backlog(&self.pool, request_id, since).await?;
        let backlog_stream = futures::stream::iter(backlog_envelopes);
        let live_filtered = filter_live_stream(live_rx, backlog_max);
        Ok(Box::pin(backlog_stream.chain(live_filtered)))
    }
}

/// Replay every chunk persisted for `request_id` strictly after `since`
/// (or every chunk if `since` is `None`). Returns the envelopes already
/// wrapped as stream events plus the maximum seq seen (used downstream
/// to dedupe against the live broadcast).
///
/// Privileged: backlog replay is triggered either by an HTTP request
/// (already gated by the caller's `begin_as` tenant-visibility check)
/// or by worker-side reconciliation. The chunks table is RLS-forced;
/// without `begin_privileged` the worker's anonymous role would filter
/// to no rows.
async fn read_backlog(
    pool: &PgPool,
    request_id: PromptRequestId,
    since: Option<ChunkSeq>,
) -> Result<(Vec<Result<StreamEvent, ResponseError>>, Option<ChunkSeq>), ResponseError> {
    let mut tx = crate::auth::begin_privileged(pool).await?;
    // Two SQL paths because the `since = None` (full replay) case has no
    // lower bound — there is no sentinel `ChunkSeq` below `ChunkSeq::ZERO`.
    let backlog: Vec<(ChunkSeq, sqlx::types::JsonValue)> = match since {
        Some(s) => {
            sqlx::query_as(
                "SELECT seq, payload FROM prompt_response_chunks
                 WHERE request_id = $1 AND seq > $2
                 ORDER BY seq ASC",
            )
            .bind(request_id)
            .bind(s)
            .fetch_all(&mut *tx)
            .await?
        }
        None => {
            sqlx::query_as(
                "SELECT seq, payload FROM prompt_response_chunks
                 WHERE request_id = $1
                 ORDER BY seq ASC",
            )
            .bind(request_id)
            .fetch_all(&mut *tx)
            .await?
        }
    };
    tx.commit().await?;

    let mut envelopes = Vec::with_capacity(backlog.len());
    let mut max_seq: Option<ChunkSeq> = since;
    for (seq, payload) in backlog {
        let chunk: ResponseChunk = serde_json::from_value(payload)
            .map_err(|e| ResponseError::Backend(format!("deserialize chunk: {e}")))?;
        envelopes.push(Ok(StreamEvent::Chunk(ResponseChunkEnvelope { seq, chunk })));
        max_seq = Some(max_seq.map_or(seq, |m| m.max(seq)));
    }
    Ok((envelopes, max_seq))
}

/// Adapt the live broadcast into a stream of `StreamEvent`s, dropping
/// any envelope whose seq was already delivered via the backlog
/// replay (overlap dedupe). `BroadcastStreamRecvError::Lagged` is
/// surfaced as a [`StreamEvent::Stalled`] sentinel so callers can
/// react instead of silently losing chunks.
fn filter_live_stream(
    live_rx: broadcast::Receiver<ResponseChunkEnvelope>,
    dedupe_threshold: Option<ChunkSeq>,
) -> impl futures::Stream<Item = Result<StreamEvent, ResponseError>> + Send {
    let live_mapped = BroadcastStream::new(live_rx).map(
        move |item| -> Result<Option<StreamEvent>, ResponseError> {
            match item {
                Ok(env) => {
                    if dedupe_threshold.is_some_and(|threshold| env.seq <= threshold) {
                        Ok(None)
                    } else {
                        Ok(Some(StreamEvent::Chunk(env)))
                    }
                }
                Err(BroadcastStreamRecvError::Lagged(_)) => Ok(Some(StreamEvent::Stalled)),
            }
        },
    );
    live_mapped.filter_map(|r| async move { r.transpose() })
}
