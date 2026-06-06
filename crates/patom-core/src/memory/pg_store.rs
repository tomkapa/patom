//! Postgres-backed [`MemoryStore`].
//!
//! All wall-clock values come from the injected [`SharedClock`] (CLAUDE.md
//! §11). Status enums and ids cross the SQL boundary via the `sqlx::Type`
//! impls in [`super::types`]; no hand-rolled string matching survives here.
//!
//! Every `Write` / `Update` synchronously embeds new content through
//! [`SharedEmbeddingProvider`] before opening the journal transaction, so a
//! materialized row never lands without a vector for retrieval to match.

use std::fmt;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sqlx::{PgPool, Row};

use crate::agents::AgentId;
use crate::auth::{UserId, run_as_user, run_privileged};
use crate::clock::SharedClock;
use crate::pg_vector;
use crate::provider::SharedEmbeddingProvider;
use crate::runtime::PromptRequestId;

use super::limits::{MAX_EVENTS_PER_PAGE, MAX_MEMORIES_PER_AGENT, MAX_SIMILAR_PAIRS_PER_AGENT};
use super::store::{
    ContradictionEventRow, MemoryEvent, MemoryEventPayload, MemoryMutation, MemoryRow, MemoryStore,
    MemoryStoreError, MutationOutcome, MutationSource, PairCandidate, ResolutionOutcome,
    ScoredMemoryRow, SearchFilter, ValidationOrigin,
};
use super::types::{
    ContradictionEventId, MemoryContent, MemoryEventId, MemoryId, MemoryKind, MemoryState,
    MutationKind, MutationSourceKind, ValidationEventId,
};

/// Column list reused by every `agent_memories` SELECT — keeping it in one
/// place removes drift between `list` / `get` / `lock_existing` and the
/// `RETURNING` clauses on the mutation paths.
const MEMORY_ROW_COLUMNS: &str = "id, agent_id, org_id, kind, content, state, pinned, \
                                  source_turn_id, created_at, last_validated_at, \
                                  last_accessed_at, access_count";

const MEMORY_EVENT_COLUMNS: &str = "id, agent_id, org_id, mutation, target_memory_id, \
                                    content_before, content_after, source_kind, source_turn_id, \
                                    created_at, kind, state, pinned";

pub struct PgMemoryStore {
    pool: PgPool,
    clock: SharedClock,
    embeddings: SharedEmbeddingProvider,
}

impl PgMemoryStore {
    #[must_use]
    pub fn new(pool: PgPool, clock: SharedClock, embeddings: SharedEmbeddingProvider) -> Self {
        Self {
            pool,
            clock,
            embeddings,
        }
    }

    fn now(&self) -> DateTime<Utc> {
        self.clock.now_utc()
    }

    /// Embed `content` synchronously. Errors propagate so the mutation
    /// aborts before journaling — no orphaned rows without embeddings.
    async fn embed(&self, content: &str) -> Result<Vec<f32>, MemoryStoreError> {
        crate::provider::embed_one(self.embeddings.as_ref(), content)
            .await
            .map_err(MemoryStoreError::Provider)
    }
}

impl fmt::Debug for PgMemoryStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PgMemoryStore").finish_non_exhaustive()
    }
}

#[async_trait]
#[allow(clippy::too_many_lines)] // exhaustive dispatch on a 3-arm enum
impl MemoryStore for PgMemoryStore {
    async fn apply(&self, mutation: MemoryMutation) -> Result<MutationOutcome, MemoryStoreError> {
        let embedding = embed_for_mutation(self, &mutation).await?;
        run_privileged(&self.pool, async |tx| {
            apply_in_tx(self, tx.tx_mut(), mutation, embedding).await
        })
        .await
    }

    async fn apply_for_user(
        &self,
        acting_user_id: UserId,
        mutation: MemoryMutation,
    ) -> Result<MutationOutcome, MemoryStoreError> {
        let embedding = embed_for_mutation(self, &mutation).await?;
        run_as_user(&self.pool, acting_user_id, async |tx| {
            apply_in_tx(self, tx.tx_mut(), mutation, embedding).await
        })
        .await
    }

    async fn record_validation_for_user(
        &self,
        acting_user_id: UserId,
        agent: AgentId,
        memory: MemoryId,
        origin: ValidationOrigin,
        detail: Option<&str>,
    ) -> Result<MemoryRow, MemoryStoreError> {
        run_as_user(&self.pool, acting_user_id, async |tx| {
            record_validation_in_tx(self, tx.tx_mut(), agent, memory, origin, detail).await
        })
        .await
    }

    async fn resolve_contradiction_for_user(
        &self,
        acting_user_id: UserId,
        id: ContradictionEventId,
        outcome: ResolutionOutcome,
    ) -> Result<(), MemoryStoreError> {
        run_as_user(&self.pool, acting_user_id, async |tx| {
            resolve_contradiction_in_tx(self, tx.tx_mut(), id, outcome).await
        })
        .await
    }

    async fn record_access_for_user(
        &self,
        acting_user_id: UserId,
        ids: &[MemoryId],
    ) -> Result<(), MemoryStoreError> {
        run_as_user(&self.pool, acting_user_id, async |tx| {
            record_access_in_tx(self, tx.tx_mut(), ids).await
        })
        .await
    }

    async fn list(&self, agent: AgentId) -> Result<Vec<MemoryRow>, MemoryStoreError> {
        // Probe one row past the cap so the assertion catches a writer that
        // violated the same limit it enforces.
        let probe_limit = i64::try_from(MAX_MEMORIES_PER_AGENT)
            .expect("invariant: MAX_MEMORIES_PER_AGENT fits in i64")
            + 1;
        let sql = format!(
            "SELECT {MEMORY_ROW_COLUMNS} FROM agent_memories
             WHERE agent_id = $1
             ORDER BY created_at ASC, id ASC
             LIMIT $2",
        );
        // Privileged tx: the store has no `Principal` in scope, and
        // `agent_memories` is RLS-forced post-migration-17. Tenant scope
        // is provided by the explicit `agent_id` bind plus the row's
        // denormalised `org_id` (parity-checked on insert).
        let mut tx = crate::auth::begin_privileged(&self.pool).await?;
        let rows = sqlx::query(&sql)
            .bind(agent)
            .bind(probe_limit)
            .fetch_all(&mut *tx)
            .await?;
        tx.commit().await?;

        assert!(
            rows.len() <= MAX_MEMORIES_PER_AGENT,
            "invariant: per-agent memory cap {MAX_MEMORIES_PER_AGENT} overshot ({})",
            rows.len()
        );

        let mut out = Vec::with_capacity(rows.len());
        for row in rows {
            out.push(decode_memory_row(&row)?);
        }
        Ok(out)
    }

    async fn get(&self, id: MemoryId) -> Result<Option<MemoryRow>, MemoryStoreError> {
        let sql = format!("SELECT {MEMORY_ROW_COLUMNS} FROM agent_memories WHERE id = $1");
        let mut tx = crate::auth::begin_privileged(&self.pool).await?;
        let row = sqlx::query(&sql).bind(id).fetch_optional(&mut *tx).await?;
        tx.commit().await?;
        row.as_ref().map(decode_memory_row).transpose()
    }

    async fn list_events(&self, agent: AgentId) -> Result<Vec<MemoryEvent>, MemoryStoreError> {
        let limit =
            i64::try_from(MAX_EVENTS_PER_PAGE).expect("invariant: MAX_EVENTS_PER_PAGE fits in i64");
        let sql = format!(
            "SELECT {MEMORY_EVENT_COLUMNS} FROM memory_events
             WHERE agent_id = $1
             ORDER BY created_at ASC, id ASC
             LIMIT $2",
        );
        let mut tx = crate::auth::begin_privileged(&self.pool).await?;
        let rows = sqlx::query(&sql)
            .bind(agent)
            .bind(limit)
            .fetch_all(&mut *tx)
            .await?;
        tx.commit().await?;

        let mut out = Vec::with_capacity(rows.len());
        for row in rows {
            out.push(decode_memory_event(&row)?);
        }
        Ok(out)
    }

    async fn rebuild_materialized(&self, agent: AgentId) -> Result<(), MemoryStoreError> {
        let mut tx = crate::auth::begin_privileged(&self.pool).await?;

        // Snapshot embeddings before DELETE so we can re-attach them after
        // replay. `memory_events` does not carry the embedding (it's
        // derived from content and lives only in the materialized view),
        // so a naive DELETE-then-replay would leave every row with
        // `embedding IS NULL` and silently break `search_by_embedding`
        // / `similar_pairs`. Snapshot is keyed by memory id; replay
        // re-creates the same ids from `event.target_memory_id`, so the
        // restore-by-id maps the right vector to the right row.
        let embedding_snapshot: Vec<(MemoryId, String)> = sqlx::query_as(
            "SELECT id, embedding::text FROM agent_memories
             WHERE agent_id = $1 AND embedding IS NOT NULL",
        )
        .bind(agent)
        .fetch_all(&mut *tx)
        .await?;

        sqlx::query("DELETE FROM agent_memories WHERE agent_id = $1")
            .bind(agent)
            .execute(&mut *tx)
            .await?;

        let sql = format!(
            "SELECT {MEMORY_EVENT_COLUMNS} FROM memory_events
             WHERE agent_id = $1
             ORDER BY created_at ASC, id ASC",
        );
        let event_rows = sqlx::query(&sql).bind(agent).fetch_all(&mut *tx).await?;

        for row in event_rows {
            let event = decode_memory_event(&row)?;
            apply_replay(&mut tx, agent, &event).await?;
        }

        // Restore embeddings for rows that still exist (Forget events
        // dropped some during replay; the UPDATE no-ops for those).
        for (id, embedding) in embedding_snapshot {
            sqlx::query("UPDATE agent_memories SET embedding = $1::vector WHERE id = $2")
                .bind(embedding)
                .bind(id)
                .execute(&mut *tx)
                .await?;
        }

        tx.commit().await?;
        Ok(())
    }

    async fn search_by_embedding(
        &self,
        agent: AgentId,
        embedding: &[f32],
        k: usize,
        filter: SearchFilter,
    ) -> Result<Vec<ScoredMemoryRow>, MemoryStoreError> {
        if embedding.is_empty() || k == 0 {
            return Ok(Vec::new());
        }
        let limit = i64::try_from(k.min(MAX_MEMORIES_PER_AGENT))
            .expect("invariant: k ≤ MAX_MEMORIES_PER_AGENT");

        let kinds_filter: Vec<String> = filter.kinds.as_ref().map_or_else(Vec::new, |ks| {
            ks.iter().map(|k| k.as_str().to_owned()).collect()
        });
        let min_state_priority: Option<i32> = filter.min_state.map(|s| i32::from(s.priority()));

        let sql = format!(
            "SELECT {MEMORY_ROW_COLUMNS},
                    1 - (embedding <=> $2::vector) AS similarity
             FROM agent_memories
             WHERE agent_id = $1
               AND embedding IS NOT NULL
               AND ($3::text[] IS NULL OR kind = ANY($3))
               AND ($4::int IS NULL OR
                    CASE state
                      WHEN 'core'      THEN 4
                      WHEN 'validated' THEN 3
                      WHEN 'held'      THEN 2
                      WHEN 'tentative' THEN 1
                    END >= $4)
             ORDER BY embedding <=> $2::vector ASC
             LIMIT $5",
        );

        let kinds_arg = if kinds_filter.is_empty() {
            None
        } else {
            Some(kinds_filter)
        };

        let mut tx = crate::auth::begin_privileged(&self.pool).await?;
        let rows = sqlx::query(&sql)
            .bind(agent)
            .bind(pg_vector::encode(embedding))
            .bind(kinds_arg)
            .bind(min_state_priority)
            .bind(limit)
            .fetch_all(&mut *tx)
            .await?;
        tx.commit().await?;

        let mut out = Vec::with_capacity(rows.len());
        for row in rows {
            let similarity: f64 = row.try_get("similarity")?;
            out.push(ScoredMemoryRow {
                row: decode_memory_row(&row)?,
                similarity: f64_to_f32_clamped(similarity),
            });
        }
        Ok(out)
    }

    async fn similar_pairs(
        &self,
        agent: AgentId,
        threshold: f32,
        max_pairs: usize,
    ) -> Result<Vec<PairCandidate>, MemoryStoreError> {
        let limit = i64::try_from(max_pairs.min(MAX_SIMILAR_PAIRS_PER_AGENT))
            .expect("invariant: max_pairs fits in i64");
        let f64_threshold = f64::from(threshold);

        // Each side is decoded by suffixing the column list with `_a` / `_b`
        // and routing through `decode_memory_row_with_suffix`. The view
        // (`pair_a`, `pair_b`) builds two SELECT lists with matching aliases
        // so one row carries both sides.
        let sql = format!(
            "WITH base AS (
                 SELECT {MEMORY_ROW_COLUMNS}, embedding FROM agent_memories
                 WHERE agent_id = $1 AND embedding IS NOT NULL
             )
             SELECT {a_cols},
                    {b_cols},
                    1 - (a.embedding <=> b.embedding) AS similarity
             FROM base a
             JOIN base b ON a.id < b.id
             WHERE 1 - (a.embedding <=> b.embedding) >= $2
             ORDER BY similarity DESC
             LIMIT $3",
            a_cols = aliased_columns("a", "a"),
            b_cols = aliased_columns("b", "b"),
        );

        let mut tx = crate::auth::begin_privileged(&self.pool).await?;
        let rows = sqlx::query(&sql)
            .bind(agent)
            .bind(f64_threshold)
            .bind(limit)
            .fetch_all(&mut *tx)
            .await?;
        tx.commit().await?;

        let mut out = Vec::with_capacity(rows.len());
        for row in rows {
            let similarity: f64 = row.try_get("similarity")?;
            out.push(PairCandidate {
                a: decode_memory_row_with_suffix(&row, "a")?,
                b: decode_memory_row_with_suffix(&row, "b")?,
                similarity: f64_to_f32_clamped(similarity),
            });
        }
        Ok(out)
    }

    async fn decay_validated(
        &self,
        agent: AgentId,
        cutoff: DateTime<Utc>,
    ) -> Result<usize, MemoryStoreError> {
        let now = self.now();
        let mut tx = crate::auth::begin_privileged(&self.pool).await?;

        let sql = format!(
            "SELECT {MEMORY_ROW_COLUMNS} FROM agent_memories
             WHERE agent_id = $1 AND state = 'validated' AND pinned = FALSE
               AND last_validated_at < $2
             FOR UPDATE",
        );
        let rows = sqlx::query(&sql)
            .bind(agent)
            .bind(cutoff)
            .fetch_all(&mut *tx)
            .await?;

        let mut demoted = 0usize;
        for row in rows {
            let parsed = decode_memory_row(&row)?;
            let payload = MemoryEventPayload::Update {
                before: parsed.content.clone(),
                after: parsed.content.clone(),
                kind: parsed.kind,
                state: MemoryState::Held,
                pinned: parsed.pinned,
            };
            insert_event(
                &mut tx,
                MemoryEventId::new(),
                parsed.agent_id,
                parsed.id,
                MutationSource::Librarian,
                now,
                &payload,
            )
            .await?;
            sqlx::query("UPDATE agent_memories SET state = 'held' WHERE id = $1")
                .bind(parsed.id)
                .execute(&mut *tx)
                .await?;
            demoted += 1;
        }

        tx.commit().await?;
        Ok(demoted)
    }

    async fn mature_tentative(
        &self,
        agent: AgentId,
        cutoff: DateTime<Utc>,
    ) -> Result<usize, MemoryStoreError> {
        let now = self.now();
        let mut tx = crate::auth::begin_privileged(&self.pool).await?;

        // Pinned rows are excluded for symmetry with decay_validated —
        // pinning is an authority signal, so the operator has already
        // chosen the state.
        let sql = format!(
            "SELECT {MEMORY_ROW_COLUMNS} FROM agent_memories
             WHERE agent_id = $1 AND state = 'tentative' AND pinned = FALSE
               AND created_at < $2
               AND NOT EXISTS (
                   SELECT 1 FROM contradiction_events ce
                   WHERE ce.resolved_at IS NULL
                     AND (ce.memory_a = agent_memories.id
                          OR ce.memory_b = agent_memories.id)
               )
             FOR UPDATE",
        );
        let rows = sqlx::query(&sql)
            .bind(agent)
            .bind(cutoff)
            .fetch_all(&mut *tx)
            .await?;

        let mut matured = 0usize;
        for row in rows {
            let parsed = decode_memory_row(&row)?;
            let payload = MemoryEventPayload::Update {
                before: parsed.content.clone(),
                after: parsed.content.clone(),
                kind: parsed.kind,
                state: MemoryState::Held,
                pinned: parsed.pinned,
            };
            insert_event(
                &mut tx,
                MemoryEventId::new(),
                parsed.agent_id,
                parsed.id,
                MutationSource::Librarian,
                now,
                &payload,
            )
            .await?;
            // State only — last_validated_at stays put. Maturation is
            // absence-of-refutation, not independent verification.
            sqlx::query("UPDATE agent_memories SET state = 'held' WHERE id = $1")
                .bind(parsed.id)
                .execute(&mut *tx)
                .await?;
            matured += 1;
        }

        tx.commit().await?;
        Ok(matured)
    }

    async fn record_validation(
        &self,
        agent: AgentId,
        memory: MemoryId,
        origin: ValidationOrigin,
        detail: Option<&str>,
    ) -> Result<MemoryRow, MemoryStoreError> {
        run_privileged(&self.pool, async |tx| {
            record_validation_in_tx(self, tx.tx_mut(), agent, memory, origin, detail).await
        })
        .await
    }

    async fn record_contradiction(
        &self,
        agent: AgentId,
        a: MemoryId,
        b: MemoryId,
        reason: &str,
    ) -> Result<ContradictionEventId, MemoryStoreError> {
        // Canonicalise the pair so duplicate detection works regardless of
        // call order.
        let (lo, hi) = if a.as_uuid() < b.as_uuid() {
            (a, b)
        } else {
            (b, a)
        };
        let now = self.now();

        // Privileged tx — `contradiction_events` is RLS-forced; the
        // store has no `Principal` in scope. `org_id` is derived from
        // the parent agent and parity-checked by the trigger.
        let mut tx = crate::auth::begin_privileged(&self.pool).await?;
        // Idempotency: if an unresolved row already exists for this pair,
        // return its id.
        // Typed lookup — `ContradictionEventId` round-trips via the
        // `uuid_newtype!` sqlx impls; never traffic in raw `uuid::Uuid`
        // at the app boundary (CLAUDE.md §1).
        let existing: Option<ContradictionEventId> = sqlx::query_scalar(
            "SELECT id FROM contradiction_events
             WHERE agent_id = $1 AND memory_a = $2 AND memory_b = $3 AND resolved_at IS NULL
             ORDER BY created_at DESC LIMIT 1",
        )
        .bind(agent)
        .bind(lo)
        .bind(hi)
        .fetch_optional(&mut *tx)
        .await?;
        if let Some(id) = existing {
            tx.commit().await?;
            return Ok(id);
        }

        let id = ContradictionEventId::new();
        sqlx::query(
            "INSERT INTO contradiction_events
                 (id, agent_id, org_id, memory_a, memory_b, reason, created_at)
             VALUES ($1, $2, (SELECT org_id FROM agents WHERE id = $2),
                     $3, $4, $5, $6)",
        )
        .bind(id)
        .bind(agent)
        .bind(lo)
        .bind(hi)
        .bind(reason)
        .bind(now)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(id)
    }

    async fn unresolved_contradictions(
        &self,
        agent: AgentId,
    ) -> Result<Vec<ContradictionEventRow>, MemoryStoreError> {
        let limit = i64::try_from(MAX_SIMILAR_PAIRS_PER_AGENT).expect("invariant: cap fits in i64");
        let mut tx = crate::auth::begin_privileged(&self.pool).await?;
        let rows = sqlx::query(
            "SELECT id, agent_id, org_id, memory_a, memory_b, reason, created_at,
                    resolved_at, resolution_event_id, resolution_reason
             FROM contradiction_events
             WHERE agent_id = $1 AND resolved_at IS NULL
             ORDER BY created_at ASC
             LIMIT $2",
        )
        .bind(agent)
        .bind(limit)
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;

        let mut out = Vec::with_capacity(rows.len());
        for row in rows {
            out.push(decode_contradiction_row(&row)?);
        }
        Ok(out)
    }

    async fn read_contradiction(
        &self,
        id: ContradictionEventId,
    ) -> Result<Option<ContradictionEventRow>, MemoryStoreError> {
        let mut tx = crate::auth::begin_privileged(&self.pool).await?;
        let row = sqlx::query(
            "SELECT id, agent_id, org_id, memory_a, memory_b, reason, created_at,
                    resolved_at, resolution_event_id, resolution_reason
             FROM contradiction_events WHERE id = $1",
        )
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?;
        tx.commit().await?;
        row.as_ref().map(decode_contradiction_row).transpose()
    }

    async fn resolve_contradiction(
        &self,
        id: ContradictionEventId,
        outcome: ResolutionOutcome,
    ) -> Result<(), MemoryStoreError> {
        run_privileged(&self.pool, async |tx| {
            resolve_contradiction_in_tx(self, tx.tx_mut(), id, outcome).await
        })
        .await
    }

    async fn evict_overflow(
        &self,
        agent: AgentId,
        quota: usize,
    ) -> Result<Vec<MemoryId>, MemoryStoreError> {
        let now = self.now();
        let mut tx = crate::auth::begin_privileged(&self.pool).await?;

        let count: (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM agent_memories WHERE agent_id = $1")
                .bind(agent)
                .fetch_one(&mut *tx)
                .await?;
        let total = usize::try_from(count.0).expect("invariant: COUNT non-negative");
        if total <= quota {
            tx.commit().await?;
            return Ok(Vec::new());
        }
        let evict_limit = i64::try_from(total - quota).expect("invariant: target fits in i64");

        // Lowest state first, then least recently / least frequently used.
        // Pinned rows are excluded entirely.
        let sql = format!(
            "SELECT {MEMORY_ROW_COLUMNS} FROM agent_memories
             WHERE agent_id = $1 AND pinned = FALSE
             ORDER BY
               CASE state
                 WHEN 'core'      THEN 4
                 WHEN 'validated' THEN 3
                 WHEN 'held'      THEN 2
                 WHEN 'tentative' THEN 1
               END ASC,
               last_accessed_at ASC,
               access_count ASC
             LIMIT $2",
        );
        let rows = sqlx::query(&sql)
            .bind(agent)
            .bind(evict_limit)
            .fetch_all(&mut *tx)
            .await?;

        let mut evicted = Vec::with_capacity(rows.len());
        for row in rows {
            let parsed = decode_memory_row(&row)?;
            let payload = MemoryEventPayload::Forget {
                before: parsed.content.clone(),
            };
            insert_event(
                &mut tx,
                MemoryEventId::new(),
                agent,
                parsed.id,
                MutationSource::Librarian,
                now,
                &payload,
            )
            .await?;
            sqlx::query("DELETE FROM agent_memories WHERE id = $1")
                .bind(parsed.id)
                .execute(&mut *tx)
                .await?;
            evicted.push(parsed.id);
        }

        tx.commit().await?;
        Ok(evicted)
    }

    async fn revert_event(
        &self,
        agent: AgentId,
        event: MemoryEventId,
    ) -> Result<Option<MemoryRow>, MemoryStoreError> {
        let now = self.now();
        let mut tx = crate::auth::begin_privileged(&self.pool).await?;

        let evt = load_event(&mut tx, agent, event).await?;
        let target = evt.target_memory_id;
        // Inverse mapping (payload variants carry everything replay needs —
        // no defensive defaults, no extra journal scans):
        //   Write   -> Forget(before = current content)
        //   Update  -> Update(before = current, after = original.before, attrs = original prior)
        //   Forget  -> Write(content = original.before, attrs = original prior)
        let inverse_event = MemoryEventId::new();
        match evt.payload {
            MemoryEventPayload::Write { .. } => {
                revert_write(&mut tx, agent, target, inverse_event, now).await?;
            }
            MemoryEventPayload::Update {
                before,
                kind,
                state,
                pinned,
                ..
            } => {
                revert_update(
                    &mut tx,
                    agent,
                    target,
                    inverse_event,
                    now,
                    before,
                    kind,
                    state,
                    pinned,
                )
                .await?;
            }
            MemoryEventPayload::Forget { before } => {
                revert_forget(&mut tx, agent, target, inverse_event, now, before).await?;
            }
        }

        let sql = format!("SELECT {MEMORY_ROW_COLUMNS} FROM agent_memories WHERE id = $1");
        let row_after = sqlx::query(&sql)
            .bind(target)
            .fetch_optional(&mut *tx)
            .await?;

        tx.commit().await?;
        row_after.as_ref().map(decode_memory_row).transpose()
    }

    async fn set_pinned(
        &self,
        agent: AgentId,
        memory: MemoryId,
        pinned: bool,
    ) -> Result<MemoryRow, MemoryStoreError> {
        let now = self.now();
        let mut tx = crate::auth::begin_privileged(&self.pool).await?;
        let prior = lock_existing(&mut tx, agent, memory, MutationSource::Operator).await?;
        let payload = MemoryEventPayload::Update {
            before: prior.content.clone(),
            after: prior.content.clone(),
            kind: prior.kind,
            state: prior.state,
            pinned,
        };
        insert_event(
            &mut tx,
            MemoryEventId::new(),
            agent,
            memory,
            MutationSource::Operator,
            now,
            &payload,
        )
        .await?;
        let sql = format!(
            "UPDATE agent_memories SET pinned = $1 WHERE id = $2 RETURNING {MEMORY_ROW_COLUMNS}",
        );
        let row = sqlx::query(&sql)
            .bind(pinned)
            .bind(memory)
            .fetch_one(&mut *tx)
            .await?;
        tx.commit().await?;
        decode_memory_row(&row)
    }

    async fn record_access(&self, ids: &[MemoryId]) -> Result<(), MemoryStoreError> {
        run_privileged(&self.pool, async |tx| {
            record_access_in_tx(self, tx.tx_mut(), ids).await
        })
        .await
    }
}

/// Load the `memory_events` row to be reverted, gated by `(id, agent_id)`.
async fn load_event(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    agent: AgentId,
    event: MemoryEventId,
) -> Result<MemoryEvent, MemoryStoreError> {
    let sql = format!(
        "SELECT {MEMORY_EVENT_COLUMNS} FROM memory_events WHERE id = $1 AND agent_id = $2",
    );
    let row = sqlx::query(&sql)
        .bind(event)
        .bind(agent)
        .fetch_optional(&mut **tx)
        .await?;
    row.map_or_else(
        || Err(MemoryStoreError::EventNotFound { id: event }),
        |r| decode_memory_event(&r),
    )
}

/// Inverse of a `Write` event: forget the materialised row and journal
/// the deletion (with the now-current content captured as `before`).
async fn revert_write(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    agent: AgentId,
    target: MemoryId,
    inverse_event: MemoryEventId,
    now: DateTime<Utc>,
) -> Result<(), MemoryStoreError> {
    let prior = lock_existing(tx, agent, target, MutationSource::Operator).await?;
    let payload = MemoryEventPayload::Forget {
        before: prior.content.clone(),
    };
    insert_event(
        tx,
        inverse_event,
        agent,
        target,
        MutationSource::Operator,
        now,
        &payload,
    )
    .await?;
    sqlx::query("DELETE FROM agent_memories WHERE id = $1")
        .bind(target)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

/// Inverse of an `Update` event: write the prior `before` back into the
/// row and journal a fresh `Update` whose `before` is the now-current
/// content.
#[allow(clippy::too_many_arguments)]
async fn revert_update(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    agent: AgentId,
    target: MemoryId,
    inverse_event: MemoryEventId,
    now: DateTime<Utc>,
    before: MemoryContent,
    kind: MemoryKind,
    state: MemoryState,
    pinned: bool,
) -> Result<(), MemoryStoreError> {
    let prior = lock_existing(tx, agent, target, MutationSource::Operator).await?;
    let payload = MemoryEventPayload::Update {
        before: prior.content.clone(),
        after: before.clone(),
        kind,
        state,
        pinned,
    };
    insert_event(
        tx,
        inverse_event,
        agent,
        target,
        MutationSource::Operator,
        now,
        &payload,
    )
    .await?;
    sqlx::query("UPDATE agent_memories SET content = $1, state = $2, pinned = $3 WHERE id = $4")
        .bind(before.as_str())
        .bind(state)
        .bind(pinned)
        .bind(target)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

/// Inverse of a `Forget` event: walk the journal for the most-recent
/// lifecycle attrs, write a fresh `agent_memories` row with the prior
/// `before` content, and journal a `Write` event marking the restore.
async fn revert_forget(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    agent: AgentId,
    target: MemoryId,
    inverse_event: MemoryEventId,
    now: DateTime<Utc>,
    before: MemoryContent,
) -> Result<(), MemoryStoreError> {
    // Walk back to the most recent write/update event for this memory
    // to recover the lifecycle attrs (the forget event itself does not
    // carry them). Missing means journal corruption — surface, don't
    // default.
    let attrs = restore_attrs_for(tx, agent, target).await?;
    let payload = MemoryEventPayload::Write {
        content: before.clone(),
        kind: attrs.kind,
        state: attrs.state,
        pinned: attrs.pinned,
    };
    insert_event(
        tx,
        inverse_event,
        agent,
        target,
        MutationSource::Operator,
        now,
        &payload,
    )
    .await?;
    // `org_id` derived from the parent agent via subquery — see
    // `apply_write` for the rationale. The
    // `agent_memories_enforce_org` trigger parity-checks it.
    sqlx::query(
        "INSERT INTO agent_memories
             (id, agent_id, org_id, kind, content, state, pinned,
              source_turn_id,
              created_at, last_validated_at, last_accessed_at, access_count)
         VALUES ($1, $2, (SELECT org_id FROM agents WHERE id = $2),
                 $3, $4, $5, $6, NULL, $7, $7, $7, 0)
         ON CONFLICT (id) DO NOTHING",
    )
    .bind(target)
    .bind(agent)
    .bind(attrs.kind)
    .bind(before.as_str())
    .bind(attrs.state)
    .bind(attrs.pinned)
    .bind(now)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// Body of `apply` / `apply_for_user`. Opens the tx via `scope` so the
/// journal + materialized writes run privileged (librarian sweeps) or
/// RLS-checked (worker / tool mutations).
/// Embed the mutation content. Runs before any `run_*` runner so the
/// network call doesn't hold row locks. `Forget` touches no content so
/// it returns `None`.
async fn embed_for_mutation(
    store: &PgMemoryStore,
    mutation: &MemoryMutation,
) -> Result<Option<Vec<f32>>, MemoryStoreError> {
    match mutation {
        MemoryMutation::Write { content, .. } | MemoryMutation::Update { content, .. } => {
            Ok(Some(store.embed(content.as_str()).await?))
        }
        MemoryMutation::Forget { .. } => Ok(None),
    }
}

async fn apply_in_tx(
    store: &PgMemoryStore,
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    mutation: MemoryMutation,
    embedding: Option<Vec<f32>>,
) -> Result<MutationOutcome, MemoryStoreError> {
    let now = store.now();
    let outcome = match mutation {
        MemoryMutation::Write {
            agent,
            kind,
            content,
            state,
            pinned,
            source,
        } => {
            let embedding = embedding.expect("invariant: Write produced an embedding above");
            apply_write(
                tx, agent, kind, content, state, pinned, source, embedding, now,
            )
            .await?
        }
        MemoryMutation::Update {
            agent,
            target,
            content,
            state,
            source,
        } => {
            let embedding = embedding.expect("invariant: Update produced an embedding above");
            apply_update(tx, agent, target, content, state, source, embedding, now).await?
        }
        MemoryMutation::Forget {
            agent,
            target,
            source,
        } => apply_forget(tx, agent, target, source, now).await?,
    };
    Ok(outcome)
}

/// Body of `record_validation` / `record_validation_for_user`. The
/// runner owns commit/rollback; this helper just runs SQL.
async fn record_validation_in_tx(
    store: &PgMemoryStore,
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    agent: AgentId,
    memory: MemoryId,
    origin: ValidationOrigin,
    detail: Option<&str>,
) -> Result<MemoryRow, MemoryStoreError> {
    let now = store.now();
    let wrapping_source = origin.mutation_source();

    let prior = lock_existing(tx, agent, memory, wrapping_source).await?;

    sqlx::query(
        "INSERT INTO validation_events
             (id, agent_id, org_id, memory_id, source, detail, created_at)
         VALUES ($1, $2, (SELECT org_id FROM agents WHERE id = $2),
                 $3, $4, $5, $6)",
    )
    .bind(ValidationEventId::new())
    .bind(agent)
    .bind(memory)
    .bind(origin.source().as_str())
    .bind(detail)
    .bind(now)
    .execute(&mut **tx)
    .await?;

    let new_state = match prior.state {
        MemoryState::Tentative => MemoryState::Held,
        MemoryState::Held => MemoryState::Validated,
        MemoryState::Validated | MemoryState::Core => prior.state,
    };

    let payload = MemoryEventPayload::Update {
        before: prior.content.clone(),
        after: prior.content.clone(),
        kind: prior.kind,
        state: new_state,
        pinned: prior.pinned,
    };
    insert_event(
        tx,
        MemoryEventId::new(),
        agent,
        memory,
        wrapping_source,
        now,
        &payload,
    )
    .await?;

    let sql = format!(
        "UPDATE agent_memories
         SET state = $1, last_validated_at = $2
         WHERE id = $3
         RETURNING {MEMORY_ROW_COLUMNS}",
    );
    let row = sqlx::query(&sql)
        .bind(new_state)
        .bind(now)
        .bind(memory)
        .fetch_one(&mut **tx)
        .await?;
    decode_memory_row(&row)
}

/// Body of `resolve_contradiction` / `resolve_contradiction_for_user`.
async fn resolve_contradiction_in_tx(
    store: &PgMemoryStore,
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    id: ContradictionEventId,
    outcome: ResolutionOutcome,
) -> Result<(), MemoryStoreError> {
    let now = store.now();
    let (event_id, reason): (Option<MemoryEventId>, Option<String>) = match outcome {
        ResolutionOutcome::Mutation(event_id) => (Some(event_id), None),
        ResolutionOutcome::NoAction { reason } => (None, Some(reason.into_inner())),
    };
    let result = sqlx::query(
        "UPDATE contradiction_events
         SET resolved_at = $1, resolution_event_id = $2, resolution_reason = $3
         WHERE id = $4 AND resolved_at IS NULL",
    )
    .bind(now)
    .bind(event_id)
    .bind(reason)
    .bind(id)
    .execute(&mut **tx)
    .await?;
    // Zero rows affected has two distinct meanings that we must
    // disambiguate: (1) the row is already resolved — idempotent
    // success, preserving the original stamped time (a contract the
    // librarian and `resolve_contradiction_is_idempotent` test both
    // rely on); (2) the row doesn't exist, or is filtered out by RLS
    // in tenant scope — real error, must surface so retry-loops
    // don't mistake silent skips for resolution.
    if result.rows_affected() == 0 {
        let exists: bool =
            sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM contradiction_events WHERE id = $1)")
                .bind(id)
                .fetch_one(&mut **tx)
                .await?;
        if !exists {
            return Err(MemoryStoreError::ContradictionNotFound(id));
        }
        // Row exists and was already resolved → idempotent no-op.
    }
    Ok(())
}

/// Body of `record_access` / `record_access_for_user`.
async fn record_access_in_tx(
    store: &PgMemoryStore,
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    ids: &[MemoryId],
) -> Result<(), MemoryStoreError> {
    if ids.is_empty() {
        return Ok(());
    }
    assert!(
        ids.len() <= MAX_MEMORIES_PER_AGENT,
        "invariant: record_access batch ≤ MAX_MEMORIES_PER_AGENT"
    );
    let now = store.now();
    sqlx::query(
        "UPDATE agent_memories
         SET last_accessed_at = $1, access_count = access_count + 1
         WHERE id = ANY($2)",
    )
    .bind(now)
    .bind(ids)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// Lifecycle attrs salvaged from the most recent write/update event when
/// reverting a forget. Missing means the journal is internally inconsistent
/// (a forget event without any prior write/update) — surfaced as an error
/// rather than papered over with defaults.
struct RestoredAttrs {
    kind: MemoryKind,
    state: MemoryState,
    pinned: bool,
}

async fn restore_attrs_for(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    agent: AgentId,
    target: MemoryId,
) -> Result<RestoredAttrs, MemoryStoreError> {
    let row: Option<(Option<String>, Option<String>, Option<bool>)> = sqlx::query_as(
        "SELECT kind, state, pinned FROM memory_events
         WHERE agent_id = $1 AND target_memory_id = $2 AND mutation IN ('write','update')
         ORDER BY created_at DESC, id DESC LIMIT 1",
    )
    .bind(agent)
    .bind(target)
    .fetch_optional(&mut **tx)
    .await?;
    let (kind, state, pinned) = row.ok_or(MemoryStoreError::NotFound { id: target })?;
    let kind = kind
        .as_deref()
        .and_then(MemoryKind::parse)
        .ok_or(MemoryStoreError::NotFound { id: target })?;
    let state = state
        .as_deref()
        .and_then(MemoryState::parse)
        .ok_or(MemoryStoreError::NotFound { id: target })?;
    let pinned = pinned.ok_or(MemoryStoreError::NotFound { id: target })?;
    Ok(RestoredAttrs {
        kind,
        state,
        pinned,
    })
}

#[allow(clippy::too_many_arguments)]
async fn apply_write(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    agent: AgentId,
    kind: MemoryKind,
    content: MemoryContent,
    state: MemoryState,
    pinned: bool,
    source: MutationSource,
    embedding: Vec<f32>,
    now: DateTime<Utc>,
) -> Result<MutationOutcome, MemoryStoreError> {
    let memory_id = MemoryId::new();
    let event_id = MemoryEventId::new();
    let payload = MemoryEventPayload::Write {
        content: content.clone(),
        kind,
        state,
        pinned,
    };

    insert_event(tx, event_id, agent, memory_id, source, now, &payload).await?;

    let embedding_lit = pg_vector::encode(&embedding);
    // `org_id` is derived from the parent agent via a correlated subquery
    // and parity-checked by the `agent_memories_enforce_org` trigger.
    let sql = format!(
        "INSERT INTO agent_memories
             (id, agent_id, org_id, kind, content, state, pinned,
              source_turn_id,
              created_at, last_validated_at, last_accessed_at, access_count, embedding)
         VALUES ($1, $2, (SELECT org_id FROM agents WHERE id = $2),
                 $3, $4, $5, $6, $7, $8, $8, $8, 0, $9::vector)
         RETURNING {MEMORY_ROW_COLUMNS}",
    );
    let row = sqlx::query(&sql)
        .bind(memory_id)
        .bind(agent)
        .bind(kind)
        .bind(content.as_str())
        .bind(state)
        .bind(pinned)
        .bind(source.turn_id())
        .bind(now)
        .bind(embedding_lit)
        .fetch_one(&mut **tx)
        .await?;

    Ok(MutationOutcome {
        event_id,
        memory_id,
        row: Some(decode_memory_row(&row)?),
    })
}

#[allow(clippy::too_many_arguments)]
async fn apply_update(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    agent: AgentId,
    target: MemoryId,
    content: MemoryContent,
    state: MemoryState,
    source: MutationSource,
    embedding: Vec<f32>,
    now: DateTime<Utc>,
) -> Result<MutationOutcome, MemoryStoreError> {
    let prior = lock_existing(tx, agent, target, source).await?;
    let event_id = MemoryEventId::new();
    let payload = MemoryEventPayload::Update {
        before: prior.content.clone(),
        after: content.clone(),
        kind: prior.kind,
        state,
        pinned: prior.pinned,
    };

    insert_event(tx, event_id, agent, target, source, now, &payload).await?;

    let embedding_lit = pg_vector::encode(&embedding);
    let sql = format!(
        "UPDATE agent_memories SET content = $1, state = $2, embedding = $3::vector WHERE id = $4
         RETURNING {MEMORY_ROW_COLUMNS}",
    );
    let row = sqlx::query(&sql)
        .bind(content.as_str())
        .bind(state)
        .bind(embedding_lit)
        .bind(target)
        .fetch_one(&mut **tx)
        .await?;

    Ok(MutationOutcome {
        event_id,
        memory_id: target,
        row: Some(decode_memory_row(&row)?),
    })
}

async fn apply_forget(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    agent: AgentId,
    target: MemoryId,
    source: MutationSource,
    now: DateTime<Utc>,
) -> Result<MutationOutcome, MemoryStoreError> {
    let prior = lock_existing(tx, agent, target, source).await?;
    let event_id = MemoryEventId::new();
    let payload = MemoryEventPayload::Forget {
        before: prior.content.clone(),
    };

    insert_event(tx, event_id, agent, target, source, now, &payload).await?;

    sqlx::query("DELETE FROM agent_memories WHERE id = $1")
        .bind(target)
        .execute(&mut **tx)
        .await?;

    Ok(MutationOutcome {
        event_id,
        memory_id: target,
        row: None,
    })
}

/// Single insert path for the journal — the payload variant determines which
/// columns are bound and which are NULL, mirroring the CHECK constraints.
async fn insert_event(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    event_id: MemoryEventId,
    agent: AgentId,
    target: MemoryId,
    source: MutationSource,
    now: DateTime<Utc>,
    payload: &MemoryEventPayload,
) -> Result<(), MemoryStoreError> {
    let (mutation, content_before, content_after, kind, state, pinned) = match payload {
        MemoryEventPayload::Write {
            content,
            kind,
            state,
            pinned,
        } => (
            MutationKind::Write,
            None,
            Some(content.as_str()),
            Some(*kind),
            Some(*state),
            Some(*pinned),
        ),
        MemoryEventPayload::Update {
            before,
            after,
            kind,
            state,
            pinned,
        } => (
            MutationKind::Update,
            Some(before.as_str()),
            Some(after.as_str()),
            Some(*kind),
            Some(*state),
            Some(*pinned),
        ),
        MemoryEventPayload::Forget { before } => (
            MutationKind::Forget,
            Some(before.as_str()),
            None,
            None,
            None,
            None,
        ),
    };

    // `org_id` is derived from the parent agent via a correlated subquery
    // and parity-checked by the `memory_events_enforce_org` trigger. The
    // store has no `OrgId` in scope (callers pass `AgentId` only), so the
    // store sources tenancy from the agent row inside the same tx.
    sqlx::query(
        "INSERT INTO memory_events
             (id, agent_id, org_id, mutation, target_memory_id,
              content_before, content_after,
              source_kind, source_turn_id, created_at,
              kind, state, pinned)
         VALUES ($1, $2, (SELECT org_id FROM agents WHERE id = $2),
                 $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)",
    )
    .bind(event_id)
    .bind(agent)
    .bind(mutation)
    .bind(target)
    .bind(content_before)
    .bind(content_after)
    .bind(source.kind())
    .bind(source.turn_id())
    .bind(now)
    .bind(kind)
    .bind(state)
    .bind(pinned)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// Lock the materialized row for an update / forget. Verifies ownership and
/// pinned-immunity. Returns the row snapshot so callers can copy
/// `content_before` onto the journal event without a second read. The
/// `source` determines whether pinned rows are reachable
/// (`MutationSource::Operator` bypasses; everything else is rejected).
async fn lock_existing(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    agent: AgentId,
    target: MemoryId,
    source: MutationSource,
) -> Result<MemoryRow, MemoryStoreError> {
    let sql = format!("SELECT {MEMORY_ROW_COLUMNS} FROM agent_memories WHERE id = $1 FOR UPDATE");
    let row = sqlx::query(&sql)
        .bind(target)
        .fetch_optional(&mut **tx)
        .await?;

    let row = row.ok_or(MemoryStoreError::NotFound { id: target })?;
    let parsed = decode_memory_row(&row)?;

    if parsed.agent_id != agent {
        return Err(MemoryStoreError::WrongAgent { id: target, agent });
    }
    if parsed.pinned && !source.bypasses_pin() {
        return Err(MemoryStoreError::PinnedImmutable { id: target });
    }

    Ok(parsed)
}

/// Apply one journal event to the materialized table during a rebuild.
/// Mirrors the live mutation paths but skips the journal append (we are
/// reading from it) and the pinned-immunity check (replay must be
/// faithful — if the original mutation was allowed, the rebuild is too).
async fn apply_replay(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    agent: AgentId,
    event: &MemoryEvent,
) -> Result<(), MemoryStoreError> {
    match &event.payload {
        MemoryEventPayload::Write {
            content,
            kind,
            state,
            pinned,
        } => {
            // Replay mirrors the live mutation; `org_id` is derived from
            // the parent agent inside the rebuild tx for the same reason
            // the live insert does it (no `OrgId` in scope at replay).
            sqlx::query(
                "INSERT INTO agent_memories
                     (id, agent_id, org_id, kind, content, state, pinned,
                      source_turn_id,
                      created_at, last_validated_at, last_accessed_at, access_count)
                 VALUES ($1, $2, (SELECT org_id FROM agents WHERE id = $2),
                         $3, $4, $5, $6, $7, $8, $8, $8, 0)",
            )
            .bind(event.target_memory_id)
            .bind(agent)
            .bind(kind)
            .bind(content.as_str())
            .bind(state)
            .bind(pinned)
            .bind(event.source.turn_id())
            .bind(event.created_at)
            .execute(&mut **tx)
            .await?;
        }
        MemoryEventPayload::Update {
            after,
            state,
            pinned,
            ..
        } => {
            sqlx::query(
                "UPDATE agent_memories SET content = $1, state = $2, pinned = $3 WHERE id = $4",
            )
            .bind(after.as_str())
            .bind(state)
            .bind(pinned)
            .bind(event.target_memory_id)
            .execute(&mut **tx)
            .await?;
        }
        MemoryEventPayload::Forget { .. } => {
            sqlx::query("DELETE FROM agent_memories WHERE id = $1")
                .bind(event.target_memory_id)
                .execute(&mut **tx)
                .await?;
        }
    }
    Ok(())
}

fn decode_memory_row(row: &sqlx::postgres::PgRow) -> Result<MemoryRow, MemoryStoreError> {
    decode_memory_row_with_suffix(row, "")
}

/// Decode a `MemoryRow` from a row whose columns are aliased with an
/// optional underscore-prefixed suffix (e.g. `id_a`, `agent_id_a`). The
/// empty suffix decodes the canonical column names. Used by
/// `similar_pairs` to pull both sides of a join in one row.
fn decode_memory_row_with_suffix(
    row: &sqlx::postgres::PgRow,
    suffix: &str,
) -> Result<MemoryRow, MemoryStoreError> {
    let col = |c: &str| col_with_suffix(c, suffix);
    let content_raw: String = row.try_get(col("content").as_str())?;
    let access_count_raw: i64 = row.try_get(col("access_count").as_str())?;
    assert!(
        access_count_raw >= 0,
        "invariant: access_count must be non-negative, got {access_count_raw}"
    );
    let access_count =
        u64::try_from(access_count_raw).expect("invariant: non-negative i64 fits in u64");
    Ok(MemoryRow {
        id: row.try_get(col("id").as_str())?,
        agent_id: row.try_get(col("agent_id").as_str())?,
        org_id: row.try_get(col("org_id").as_str())?,
        kind: row.try_get(col("kind").as_str())?,
        content: MemoryContent::try_from(content_raw)?,
        state: row.try_get(col("state").as_str())?,
        pinned: row.try_get(col("pinned").as_str())?,
        source_turn_id: row
            .try_get::<Option<PromptRequestId>, _>(col("source_turn_id").as_str())?,
        created_at: row.try_get(col("created_at").as_str())?,
        last_validated_at: row.try_get(col("last_validated_at").as_str())?,
        last_accessed_at: row.try_get(col("last_accessed_at").as_str())?,
        access_count,
    })
}

fn col_with_suffix(name: &str, suffix: &str) -> String {
    if suffix.is_empty() {
        name.to_owned()
    } else {
        format!("{name}_{suffix}")
    }
}

/// Build the SELECT list for `similar_pairs` — every column from
/// [`MEMORY_ROW_COLUMNS`] qualified by `table.` and aliased to
/// `column_suffix` so the per-side decoder reads one row.
fn aliased_columns(table: &str, suffix: &str) -> String {
    MEMORY_ROW_COLUMNS
        .split(", ")
        .map(|col| format!("{table}.{col} AS {col}_{suffix}"))
        .collect::<Vec<_>>()
        .join(", ")
}

fn decode_memory_event(row: &sqlx::postgres::PgRow) -> Result<MemoryEvent, MemoryStoreError> {
    let mutation: MutationKind = row.try_get("mutation")?;
    let content_before_raw: Option<String> = row.try_get("content_before")?;
    let content_after_raw: Option<String> = row.try_get("content_after")?;
    let kind: Option<MemoryKind> = row.try_get("kind")?;
    let state: Option<MemoryState> = row.try_get("state")?;
    let pinned: Option<bool> = row.try_get("pinned")?;
    let source_kind: MutationSourceKind = row.try_get("source_kind")?;
    let source_turn_id: Option<PromptRequestId> = row.try_get("source_turn_id")?;

    let source = match source_kind {
        MutationSourceKind::Turn => {
            let id =
                source_turn_id.expect("invariant: source_kind = 'turn' must carry source_turn_id");
            MutationSource::Turn(id)
        }
        MutationSourceKind::Operator => MutationSource::Operator,
        MutationSourceKind::Librarian => MutationSource::Librarian,
    };

    // The DB CHECK constraints guarantee the shape per mutation kind; the
    // `expect` messages name the constraint each branch depends on.
    let payload = match mutation {
        MutationKind::Write => MemoryEventPayload::Write {
            content: MemoryContent::try_from(content_after_raw.expect(
                "invariant: memory_events_content_shape — write must carry content_after",
            ))?,
            kind: kind.expect("invariant: memory_events_payload_shape — write must carry kind"),
            state: state.expect("invariant: memory_events_payload_shape — write must carry state"),
            pinned: pinned
                .expect("invariant: memory_events_payload_shape — write must carry pinned"),
        },
        MutationKind::Update => MemoryEventPayload::Update {
            before: MemoryContent::try_from(content_before_raw.expect(
                "invariant: memory_events_content_shape — update must carry content_before",
            ))?,
            after: MemoryContent::try_from(content_after_raw.expect(
                "invariant: memory_events_content_shape — update must carry content_after",
            ))?,
            kind: kind.expect("invariant: memory_events_payload_shape — update must carry kind"),
            state: state.expect("invariant: memory_events_payload_shape — update must carry state"),
            pinned: pinned
                .expect("invariant: memory_events_payload_shape — update must carry pinned"),
        },
        MutationKind::Forget => MemoryEventPayload::Forget {
            before: MemoryContent::try_from(content_before_raw.expect(
                "invariant: memory_events_content_shape — forget must carry content_before",
            ))?,
        },
    };

    Ok(MemoryEvent {
        id: row.try_get("id")?,
        agent_id: row.try_get("agent_id")?,
        org_id: row.try_get("org_id")?,
        target_memory_id: row.try_get("target_memory_id")?,
        source,
        created_at: row.try_get("created_at")?,
        payload,
    })
}

fn decode_contradiction_row(
    row: &sqlx::postgres::PgRow,
) -> Result<ContradictionEventRow, MemoryStoreError> {
    Ok(ContradictionEventRow {
        id: row.try_get("id")?,
        agent_id: row.try_get("agent_id")?,
        org_id: row.try_get("org_id")?,
        memory_a: row.try_get("memory_a")?,
        memory_b: row.try_get("memory_b")?,
        reason: row.try_get("reason")?,
        created_at: row.try_get("created_at")?,
        resolved_at: row.try_get("resolved_at")?,
        resolution_event_id: row.try_get("resolution_event_id")?,
        resolution_reason: row.try_get("resolution_reason")?,
    })
}

/// f64 → f32 narrowing for pgvector cosine similarity. The score is in
/// `[-1, 1]` by construction, well inside f32 range; the assertion is
/// defensive for malformed embeddings.
fn f64_to_f32_clamped(v: f64) -> f32 {
    assert!(
        v.is_nan() || (-2.0..=2.0).contains(&v),
        "invariant: cosine similarity in [-1, 1]; got {v}"
    );
    #[allow(clippy::as_conversions, clippy::cast_possible_truncation)]
    let truncated = v as f32;
    truncated
}
