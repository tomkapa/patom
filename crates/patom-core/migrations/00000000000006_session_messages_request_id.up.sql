-- session_messages.request_id — anchors every row to the prompt request that
-- produced it. The web client uses this column to reconcile optimistic /
-- live-stream / persisted-history bubbles by identity instead of by text
-- matching (see doc/thread_panel_refactor_export.md).
--
-- Pre-launch: no production data, no backfill. The column is NOT NULL so the
-- type system carries the invariant — every appender threads the active
-- prompt's request_id through to the row, and the FE never has to handle a
-- "legacy NULL" case.
--
-- ON DELETE CASCADE: if a prompt request goes away, the rows it produced go
-- with it; otherwise the FK would orphan history under a stale id.

ALTER TABLE session_messages
    ADD COLUMN request_id UUID NOT NULL
        REFERENCES prompt_requests(id) ON DELETE CASCADE;

-- Lookup index used by the FE-driven dedup path (`request_id` joins live
-- bubbles to persisted rows) and by any future analytics query that scopes
-- to a single request.
CREATE INDEX session_messages_request_id_idx
    ON session_messages (request_id);
