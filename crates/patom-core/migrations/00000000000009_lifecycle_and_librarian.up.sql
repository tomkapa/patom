-- Lifecycle + librarian + embedding (doc/memory.md §2.5–§2.9 — Phases 5/6/9).
--
-- Three changes:
--
-- 1. `memory_events` denormalises `kind`, `state`, `pinned` per row so
--    replay (rebuild_materialized) reproduces them faithfully — phase 1
--    defaulted these on rebuild; phase 5 needs the journal to be the
--    full source of truth for the lifecycle state machine.
--
-- 2. `validation_events` — independent-signal log driving the
--    `Tentative → Held → Validated` promotion clock (doc/memory.md §1.7).
--    Every entry advances `agent_memories.last_validated_at` (and bumps
--    state when thresholds cross). Per §1.7 the validation clock advances
--    only on independent signals: external confirmation, operator endorsement.
--    Passive `tentative → held` maturation (doc/memory.md §1.8) is driven
--    by the librarian via the journal instead — no `validation_events`
--    row, no `last_validated_at` bump.
--
-- 3. `agent_memories` gains an explicit `forgotten_at` (held in the
--    journal as a `forget` event already; this column lets the librarian
--    eviction path stamp a row as gone without losing the materialized
--    record until the next journal replay). Pre-launch — see
--    `feedback_no_backcompat`: no rows yet so this is one-shot.

-- ───────────────────────────────────────────────────────────────────────────
-- memory_events — carry the full mutation payload so the replay path
-- reconstructs the row identically to the live mutation. `kind`, `state`,
-- `pinned` are nullable for `forget` events (the row is gone, those
-- fields would lie). For `write` and `update` events they are present.
-- Pre-launch: existing dev rows are wiped, see feedback_no_backcompat.
-- ───────────────────────────────────────────────────────────────────────────
ALTER TABLE memory_events
    ADD COLUMN kind   TEXT NULL CHECK (kind IS NULL OR kind IN ('self','other','procedure','open')),
    ADD COLUMN state  TEXT NULL CHECK (state IS NULL OR state IN ('core','validated','held','tentative')),
    ADD COLUMN pinned BOOLEAN NULL,
    -- Per-mutation invariants: write/update carry kind+state+pinned;
    -- forget leaves them NULL (the row disappears from the materialized
    -- view).
    ADD CONSTRAINT memory_events_payload_shape CHECK (
        (mutation IN ('write','update') AND kind IS NOT NULL AND state IS NOT NULL AND pinned IS NOT NULL)
     OR (mutation = 'forget' AND kind IS NULL AND state IS NULL AND pinned IS NULL)
    );

-- ───────────────────────────────────────────────────────────────────────────
-- validation_events — independent-signal log that drives the validation
-- clock. The agent (via `memory_validate`) inserts rows when an external
-- signal — a `web_search` / `web_fetch` result, a peer-agent reply, a
-- human reached via `send_message`, or the user affirming in the current
-- turn — confirms an existing memory; the operator endorsement path
-- inserts rows on pinned `manager_note` writes. The materialized memory's
-- `last_validated_at` is bumped to `created_at` on insert, plus a state
-- promotion if the threshold crosses.
-- ───────────────────────────────────────────────────────────────────────────
CREATE TABLE validation_events (
    id            UUID PRIMARY KEY,
    agent_id      UUID NOT NULL REFERENCES agents(id) ON DELETE CASCADE,
    memory_id     UUID NOT NULL REFERENCES agent_memories(id) ON DELETE CASCADE,
    -- Source of the independent signal. Constraints:
    --   `external_confirmation`  — agent's own follow-up confirmed it
    --                              (recall + web_search / web_fetch /
    --                              send_message reply / user affirmation
    --                              in the current turn).
    --   `operator_endorsement`   — manager_note path validated/pinned
    --                              the row.
    source        TEXT NOT NULL CHECK (
                      source IN ('external_confirmation', 'operator_endorsement')
                  ),
    detail        TEXT NULL CHECK (detail IS NULL OR octet_length(detail) BETWEEN 1 AND 1024),
    created_at    TIMESTAMPTZ NOT NULL
);

CREATE INDEX validation_events_memory_idx
    ON validation_events (memory_id, created_at);

CREATE INDEX validation_events_agent_idx
    ON validation_events (agent_id, created_at);

-- ───────────────────────────────────────────────────────────────────────────
-- Cosine-similarity index for the embedding column (Phase 9 retrieval).
--
-- pgvector requires a fixed dimension on the column for an HNSW/IVF index;
-- ALTER COLUMN to a sized vector now that the embedding provider is wired
-- and (per `EmbeddingProvider::dimensions`) the column dimension is known
-- at startup. We pick 1536 — `text-embedding-3-small`'s native size — as
-- the contract; alternative providers must produce the same dimension or
-- a code-side reshape is required (and is rejected at boundary by
-- `EmbeddingProvider::dimensions()`'s assertion).
-- ───────────────────────────────────────────────────────────────────────────
ALTER TABLE agent_memories
    ALTER COLUMN embedding TYPE vector(1536) USING embedding::vector(1536);

-- HNSW gives sub-linear retrieval at the per-agent quotas the librarian
-- enforces. `ops` family `vector_cosine_ops` matches the `<=>` operator
-- the contextual layer uses.
CREATE INDEX agent_memories_embedding_hnsw
    ON agent_memories USING hnsw (embedding vector_cosine_ops);
