-- Memory subsystem foundation (doc/memory.md §2.1).
--
-- Four new tables: an append-only journal (`memory_events`) drives a
-- materialized row table (`agent_memories`) through a single transactional
-- mutation function in `src/memory/pg_store.rs`. `contradiction_events` is
-- written by the librarian (Phase 6); `reflection_checkpoints` records the
-- last-processed turn per (agent, session) so reflection (Phase 4) is
-- idempotent across resumption.
--
-- Existing-table change: `prompt_requests` gains a `kind` enum and a
-- `kind_payload` JSONB so the same queue/worker pool dispatches normal,
-- reflection, and resolution turns (no fork). Both columns are NOT NULL
-- and 1:1 with each other — every kind has a payload variant
-- (`Normal {}` is empty for now, room for Normal-specific fields later
-- without a wire-format break). Pre-launch — see `feedback_no_backcompat`:
-- existing dev rows get the `'normal'` defaults in one shot.
--
-- pgvector is already enabled in migration 1. The `embedding` column on
-- `agent_memories` is nullable for Phase 1 because no embedding writer
-- exists yet — Phase 9 wires `EmbeddingProvider` and backfills on first
-- write. No vector index is created here; an index requires a fixed
-- dimension which is configured at provider time.

-- ───────────────────────────────────────────────────────────────────────────
-- prompt_requests.kind / kind_payload — generalises the queue from "queue
-- of prompts" to "queue of agent jobs". `kind = 'normal'` is the existing
-- behavior; the worker dispatches on this column to pick reply / reflect /
-- resolve. `kind_payload` carries kind-specific metadata: nothing for
-- normal, `{ session_id, since_turn_id }` for reflection,
-- `{ contradiction_event_id }` for resolution.
-- ───────────────────────────────────────────────────────────────────────────
ALTER TABLE prompt_requests
    ADD COLUMN kind         TEXT NOT NULL DEFAULT 'normal'
                            CHECK (kind IN ('normal','reflection','resolution')),
    ADD COLUMN kind_payload JSONB NOT NULL
                            DEFAULT '{"kind":"normal","data":{}}'::jsonb;

-- ───────────────────────────────────────────────────────────────────────────
-- memory_events — append-only journal. The source of truth: `agent_memories`
-- can be rebuilt from this table at any time. Every mutation in the system
-- (agent tool call, operator note, librarian sweep) appends one row here.
-- `content_before` / `content_after` are denormalised onto the event so a
-- revert (Phase 8) can be expressed as an inverse event without consulting
-- the materialized table.
-- ───────────────────────────────────────────────────────────────────────────
CREATE TABLE memory_events (
    id                UUID PRIMARY KEY,
    agent_id          UUID NOT NULL REFERENCES agents(id) ON DELETE CASCADE,
    mutation          TEXT NOT NULL CHECK (mutation IN ('write','update','forget')),
    target_memory_id  UUID NOT NULL,
    content_before    TEXT NULL,
    content_after     TEXT NULL,
    -- Provenance — one of `turn`, `operator`, `librarian`. The matching
    -- detail column carries the prompt-request id for `turn`; the others
    -- have no detail.
    source_kind       TEXT NOT NULL CHECK (source_kind IN ('turn','operator','librarian')),
    source_turn_id    UUID NULL REFERENCES prompt_requests(id) ON DELETE SET NULL,
    created_at        TIMESTAMPTZ NOT NULL,
    -- A `turn` source must carry a turn id; the other sources must not.
    CONSTRAINT memory_events_source_turn CHECK (
        (source_kind = 'turn') = (source_turn_id IS NOT NULL)
    ),
    -- Per-mutation content invariants:
    --   write  : content_before NULL,    content_after NOT NULL
    --   update : content_before NOT NULL, content_after NOT NULL
    --   forget : content_before NOT NULL, content_after NULL
    CONSTRAINT memory_events_content_shape CHECK (
        (mutation = 'write'  AND content_before IS NULL     AND content_after IS NOT NULL)
     OR (mutation = 'update' AND content_before IS NOT NULL AND content_after IS NOT NULL)
     OR (mutation = 'forget' AND content_before IS NOT NULL AND content_after IS NULL)
    )
);

-- Replay walks events for one agent in append order; orderable by created_at
-- with id as tiebreaker for events minted in the same instant.
CREATE INDEX memory_events_agent_idx
    ON memory_events (agent_id, created_at, id);

-- Per-target ordering — the librarian and revert paths read every event for
-- one memory id in chronological order.
CREATE INDEX memory_events_target_idx
    ON memory_events (target_memory_id, created_at, id);

-- ───────────────────────────────────────────────────────────────────────────
-- agent_memories — materialized table derived from the journal. Fast to
-- read; always rebuildable. Embedding is nullable until Phase 9 lands the
-- EmbeddingProvider; once populated, downstream retrieval (Phase 2/3) will
-- key off cosine similarity.
-- ───────────────────────────────────────────────────────────────────────────
CREATE TABLE agent_memories (
    id                  UUID PRIMARY KEY,
    agent_id            UUID NOT NULL REFERENCES agents(id) ON DELETE CASCADE,
    kind                TEXT NOT NULL
                        CHECK (kind IN ('self','other','procedure','open')),
    content             TEXT NOT NULL CHECK (octet_length(content) BETWEEN 1 AND 4096),
    state               TEXT NOT NULL
                        CHECK (state IN ('core','validated','held','tentative')),
    pinned              BOOLEAN NOT NULL DEFAULT FALSE,
    -- Source turn that first wrote the memory; nullable because operator
    -- notes (`source_kind = 'operator'`) and librarian merges have no turn.
    source_turn_id      UUID NULL REFERENCES prompt_requests(id) ON DELETE SET NULL,
    embedding           VECTOR NULL,
    created_at          TIMESTAMPTZ NOT NULL,
    last_validated_at   TIMESTAMPTZ NOT NULL,
    last_accessed_at    TIMESTAMPTZ NOT NULL,
    access_count        BIGINT NOT NULL DEFAULT 0 CHECK (access_count >= 0)
);

-- Stable / contextual layer assembly (Phase 2) reads every row for one
-- agent; this is the primary access path.
CREATE INDEX agent_memories_agent_idx
    ON agent_memories (agent_id);

-- ───────────────────────────────────────────────────────────────────────────
-- contradiction_events — librarian-detected pairs awaiting resolution
-- (Phase 6/7). Closed in one of two shapes:
--   * mutation close — `resolution_event_id` points at the `memory_update` /
--     `memory_forget` journal row that resolved the pair; `resolution_reason`
--     is NULL because the journal row is the audit record.
--   * no-action close — `resolution_event_id` is NULL; `resolution_reason`
--     carries the assistant's final-text rationale (the resolution turn
--     decided neither memory needed mutating).
-- A row is `pending` while all three of resolved_at / resolution_event_id /
-- resolution_reason are NULL.
-- ───────────────────────────────────────────────────────────────────────────
CREATE TABLE contradiction_events (
    id                    UUID PRIMARY KEY,
    agent_id              UUID NOT NULL REFERENCES agents(id) ON DELETE CASCADE,
    memory_a              UUID NOT NULL,
    memory_b              UUID NOT NULL,
    reason                TEXT NOT NULL CHECK (octet_length(reason) BETWEEN 1 AND 1024),
    created_at            TIMESTAMPTZ NOT NULL,
    resolved_at           TIMESTAMPTZ NULL,
    resolution_event_id   UUID NULL REFERENCES memory_events(id) ON DELETE SET NULL,
    resolution_reason     TEXT NULL CHECK (
        resolution_reason IS NULL
        OR octet_length(resolution_reason) BETWEEN 1 AND 1024
    ),
    -- The two memories must be distinct for a contradiction to be meaningful.
    CONSTRAINT contradiction_events_distinct CHECK (memory_a <> memory_b),
    -- Three valid states: pending, mutation-closed, no-action-closed. Any
    -- other combination of (resolved_at, resolution_event_id, resolution_reason)
    -- means the resolution path partially succeeded, which is a bug.
    CONSTRAINT contradiction_events_resolved_consistent CHECK (
        (resolved_at IS NULL
            AND resolution_event_id IS NULL
            AND resolution_reason IS NULL)
        OR (resolved_at IS NOT NULL
            AND resolution_event_id IS NOT NULL
            AND resolution_reason IS NULL)
        OR (resolved_at IS NOT NULL
            AND resolution_event_id IS NULL
            AND resolution_reason IS NOT NULL)
    )
);

-- Pending-pair scan: the librarian-resolution scheduler (Phase 7) walks
-- this index per agent.
CREATE INDEX contradiction_events_unresolved_idx
    ON contradiction_events (agent_id, created_at)
    WHERE resolved_at IS NULL;

-- Per-memory lookup of unresolved entanglement: the librarian's
-- maturation step (doc/memory.md §1.8) probes per row whether a tentative
-- memory is on either side of an unresolved contradiction. Two partial
-- indexes (one per side) let that NOT EXISTS subquery land on an index
-- rather than scan the table per outer row.
CREATE INDEX contradiction_events_unresolved_memory_a_idx
    ON contradiction_events (memory_a)
    WHERE resolved_at IS NULL;
CREATE INDEX contradiction_events_unresolved_memory_b_idx
    ON contradiction_events (memory_b)
    WHERE resolved_at IS NULL;

-- ───────────────────────────────────────────────────────────────────────────
-- reflection_checkpoints — per (agent, session). The reflection scheduler
-- (Phase 4) finds (agent_id, session_id) pairs whose latest turn is past
-- the checkpoint and idle for long enough, then enqueues a Reflection job
-- that processes only the new turns.
-- ───────────────────────────────────────────────────────────────────────────
CREATE TABLE reflection_checkpoints (
    agent_id              UUID NOT NULL REFERENCES agents(id) ON DELETE CASCADE,
    session_id            UUID NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    last_turn_id          UUID NOT NULL REFERENCES prompt_requests(id) ON DELETE CASCADE,
    reflection_event_id   UUID NOT NULL REFERENCES memory_events(id) ON DELETE CASCADE,
    created_at            TIMESTAMPTZ NOT NULL,
    PRIMARY KEY (agent_id, session_id)
);
