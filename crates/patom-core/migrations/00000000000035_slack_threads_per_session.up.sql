-- Per-session Slack threads.
--
-- Previously `slack_threads` carried `UNIQUE (root_request_id)`, which
-- forced every session inside a DAG to share one Slack thread. That
-- mirrors the web UI's flattened render but contradicts Slack
-- semantics: the inbound bridge enforces one thread = one agent, so a
-- multiplexed outbound thread becomes unaddressable to the user.
--
-- The backend already models `(agent, human)` as its own session via
-- `sessions_dag_pair_unique` (migration 4). This migration removes the
-- DAG-level coupling so the outbound stream pump can mint one Slack
-- thread per session — multiple rows under the same `root_request_id`,
-- each pinned to a distinct `session_id`.

ALTER TABLE slack_threads
    DROP CONSTRAINT slack_threads_root_request_id_key;

CREATE UNIQUE INDEX slack_threads_session_idx
    ON slack_threads(session_id);
