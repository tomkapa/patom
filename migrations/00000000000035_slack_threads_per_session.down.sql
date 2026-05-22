DROP INDEX IF EXISTS slack_threads_session_idx;

ALTER TABLE slack_threads
    ADD CONSTRAINT slack_threads_root_request_id_key UNIQUE (root_request_id);
