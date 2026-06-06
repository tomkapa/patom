-- Initial schema for the Postgres-backed runtime.
-- See doc/task1.md for the data-model rationale and the trait split this is wired into.

-- pgvector is preinstalled by the pgvector/pgvector docker image. Activated here so
-- a future PgMemory impl is one migration away (no Docker rebuild). No vector tables
-- are added today.
CREATE EXTENSION IF NOT EXISTS vector;

-- A single conversation. The HTTP layer mints the id (POST /sessions) and the agent
-- never holds the message vec directly — it asks SessionStore for a snapshot.
CREATE TABLE sessions (
    id            UUID PRIMARY KEY,
    created_at    TIMESTAMPTZ NOT NULL
);

-- One row per ChatMessage in append order. body holds the tagged-union envelope
-- (User | Assistant) as JSONB so adding a content variant does not require a
-- schema change. seq is per-session and assigned by the writer.
CREATE TABLE session_messages (
    session_id    UUID NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    seq           BIGINT NOT NULL,
    role          TEXT NOT NULL CHECK (role IN ('user','assistant')),
    body          JSONB NOT NULL,
    created_at    TIMESTAMPTZ NOT NULL,
    PRIMARY KEY (session_id, seq)
);

-- Queue rows. status drives the state machine (pending -> processing -> done|failed).
-- octet_length checks mirror the Rust-side caps so an oversize payload is rejected
-- by the backend even if the boundary check is bypassed.
CREATE TABLE prompt_requests (
    id                       UUID PRIMARY KEY,
    session_id               UUID NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    content                  TEXT NOT NULL CHECK (octet_length(content) <= 65536),
    idempotency_key          TEXT NOT NULL CHECK (octet_length(idempotency_key) BETWEEN 1 AND 200),
    status                   TEXT NOT NULL CHECK (status IN ('pending','processing','done','failed')),
    attempts                 INTEGER NOT NULL DEFAULT 0 CHECK (attempts >= 0),
    turn_seq                 BIGINT NOT NULL DEFAULT 0,
    cancellation_requested   BOOLEAN NOT NULL DEFAULT FALSE,
    failure_reason           TEXT,
    created_at               TIMESTAMPTZ NOT NULL,
    updated_at               TIMESTAMPTZ NOT NULL,
    -- task1.md: becomes UNIQUE (user_id, idempotency_key) when auth lands.
    UNIQUE (idempotency_key)
);

-- Partial index that claim_next_session walks via SELECT ... FOR UPDATE SKIP LOCKED.
-- Sized for the queue's hot path; full-table scans on prompt_requests are not
-- expected on the read path.
CREATE INDEX prompt_requests_pending_idx
    ON prompt_requests (session_id, created_at)
    WHERE status = 'pending';

-- At most one lease per session. Updated when claim wins, cleared on release.
CREATE TABLE session_leases (
    session_id    UUID PRIMARY KEY REFERENCES sessions(id) ON DELETE CASCADE,
    worker_id     UUID NOT NULL,
    turn_seq      BIGINT NOT NULL,
    leased_until  TIMESTAMPTZ NOT NULL
);

-- Per-session monotonically-increasing counter handed out on claim. Stored as a
-- row (rather than computed via aggregate) so the claim transaction stays bounded.
CREATE TABLE session_turn_seq (
    session_id    UUID PRIMARY KEY REFERENCES sessions(id) ON DELETE CASCADE,
    next_seq      BIGINT NOT NULL DEFAULT 0
);

-- One row per published chunk. Replayed verbatim by the SSE handler when a client
-- (re)subscribes with Last-Event-ID. Append-only; ordered by (request_id, seq).
CREATE TABLE prompt_response_chunks (
    request_id    UUID NOT NULL REFERENCES prompt_requests(id) ON DELETE CASCADE,
    seq           BIGINT NOT NULL,
    payload       JSONB NOT NULL,
    bytes         INTEGER NOT NULL,
    is_terminal   BOOLEAN NOT NULL DEFAULT FALSE,
    created_at    TIMESTAMPTZ NOT NULL,
    PRIMARY KEY (request_id, seq)
);

-- Sidecar table holding next-seq + closed flag per request. Avoids MAX(seq) on
-- every publish, and lets close() flip the closed flag without touching chunks.
CREATE TABLE prompt_response_streams (
    request_id    UUID PRIMARY KEY REFERENCES prompt_requests(id) ON DELETE CASCADE,
    next_seq      BIGINT NOT NULL DEFAULT 0,
    closed        BOOLEAN NOT NULL DEFAULT FALSE
);
