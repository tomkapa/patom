-- Restore the pre-migration shape: one Slack thread per DAG.
--
-- Preflight: if any `root_request_id` has fan-out (more than one row),
-- the `UNIQUE (root_request_id)` constraint cannot be re-added. Abort
-- with a deterministic error rather than letting `ALTER TABLE` fail
-- with a cryptic duplicate-key message; the operator must decide how
-- to contract or discard the descendant-agent threads before rolling
-- back.
DO $$
DECLARE
    dup_count BIGINT;
BEGIN
    SELECT COUNT(*) INTO dup_count
    FROM (
        SELECT root_request_id
        FROM slack_threads
        GROUP BY root_request_id
        HAVING COUNT(*) > 1
    ) AS d;
    IF dup_count > 0 THEN
        RAISE EXCEPTION
            'cannot roll back migration 35: % root_request_id values have multiple slack_threads rows; contract or delete the descendant-session bindings before re-adding UNIQUE(root_request_id)',
            dup_count;
    END IF;
END $$;

DROP INDEX IF EXISTS slack_threads_session_idx;

ALTER TABLE slack_threads
    ADD CONSTRAINT slack_threads_root_request_id_key UNIQUE (root_request_id);
