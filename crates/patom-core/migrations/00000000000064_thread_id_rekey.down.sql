-- Reverse of 64 (lossy — reset waiver). Restores the pre-64 column structure +
-- supporting index on each table; no data is recovered (the rows referenced dead
-- sessions and were already dropped on the way up).

-- ── mcp_oauth_pending ──────────────────────────────────────────────────────────
ALTER TABLE mcp_oauth_pending RENAME COLUMN thread_id TO session_id;

-- ── slack_threads ────────────────────────────────────────────────────────────
DELETE FROM slack_threads;

DROP INDEX IF EXISTS slack_threads_thread_idx;

ALTER TABLE slack_threads
    DROP COLUMN thread_id,
    ADD COLUMN root_request_id UUID NOT NULL,
    ADD COLUMN session_id      UUID NOT NULL;

-- Pre-64 shape (post-migration 35): UNIQUE on session_id, plain root_request_id.
CREATE UNIQUE INDEX slack_threads_session_idx ON slack_threads(session_id);
