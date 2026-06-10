-- Re-key the two surviving bare-UUID `session_id` columns onto the thread model.
--
-- Migration 63 dropped `sessions`/`session_messages` and introduced the thread
-- feed, but two columns referenced the now-dead session rows WITHOUT a foreign
-- key, so they survived structurally:
--
--   1. `slack_threads.session_id` + `root_request_id` (migrations 29 + 35) — the
--      Slack-thread ↔ Patom binding. Re-keyed onto a single `thread_id` FK: one
--      Slack thread ↔ one Patom thread (the pre-thread per-session fan-out is
--      gone — a Patom thread is already multi-party).
--   2. `mcp_oauth_pending.session_id` (migration 34) — the OAuth-callback resume
--      context. Renamed to `thread_id`; the resume now appends to the thread feed
--      and enqueues a trigger instead of continuing a pair-session.
--
-- Reset waiver (friends-only prod, doc/thread-chat-refactor.md §1): no backfill —
-- any surviving runtime rows reference dead sessions and are dropped. The `.down`
-- is lossy (restores structure + RLS, no data).

-- ── slack_threads ────────────────────────────────────────────────────────────
DELETE FROM slack_threads;

DROP INDEX IF EXISTS slack_threads_session_idx;

ALTER TABLE slack_threads
    DROP COLUMN session_id,
    DROP COLUMN root_request_id,
    ADD COLUMN thread_id UUID NOT NULL REFERENCES threads(id) ON DELETE CASCADE;

-- One Slack thread ↔ one Patom thread (the inbound bridge binds on first
-- mention; the outbound pump posts every chunk on that thread into it).
CREATE UNIQUE INDEX slack_threads_thread_idx ON slack_threads(thread_id);

-- ── mcp_oauth_pending ──────────────────────────────────────────────────────────
-- Bare UUID, no FK (matches the prior `session_id`). The all-or-none CHECK
-- (`num_nonnulls(session_id, agent_id) IN (0, 2)`) references the column by name;
-- RENAME rewrites the constraint expression automatically.
ALTER TABLE mcp_oauth_pending RENAME COLUMN session_id TO thread_id;
