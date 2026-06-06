-- Reverse migration 18. Drop policies → triggers → indexes → columns
-- in dependency-safe order, then the shared parity-trigger functions
-- once their last trigger is gone.

DROP POLICY IF EXISTS prompt_response_streams_org_isolation ON prompt_response_streams;
ALTER TABLE prompt_response_streams DISABLE ROW LEVEL SECURITY;
DROP TRIGGER IF EXISTS prompt_response_streams_enforce_org ON prompt_response_streams;
DROP INDEX IF EXISTS prompt_response_streams_org_idx;
ALTER TABLE prompt_response_streams DROP COLUMN IF EXISTS org_id;

DROP POLICY IF EXISTS prompt_response_chunks_org_isolation ON prompt_response_chunks;
ALTER TABLE prompt_response_chunks DISABLE ROW LEVEL SECURITY;
DROP TRIGGER IF EXISTS prompt_response_chunks_enforce_org ON prompt_response_chunks;
DROP INDEX IF EXISTS prompt_response_chunks_org_idx;
ALTER TABLE prompt_response_chunks DROP COLUMN IF EXISTS org_id;

DROP POLICY IF EXISTS session_turn_seq_org_isolation ON session_turn_seq;
ALTER TABLE session_turn_seq DISABLE ROW LEVEL SECURITY;
DROP TRIGGER IF EXISTS session_turn_seq_enforce_org ON session_turn_seq;
DROP INDEX IF EXISTS session_turn_seq_org_idx;
ALTER TABLE session_turn_seq DROP COLUMN IF EXISTS org_id;

DROP POLICY IF EXISTS session_leases_org_isolation ON session_leases;
ALTER TABLE session_leases DISABLE ROW LEVEL SECURITY;
DROP TRIGGER IF EXISTS session_leases_enforce_org ON session_leases;
DROP INDEX IF EXISTS session_leases_org_idx;
ALTER TABLE session_leases DROP COLUMN IF EXISTS org_id;

DROP POLICY IF EXISTS prompt_request_dags_org_isolation ON prompt_request_dags;
ALTER TABLE prompt_request_dags DISABLE ROW LEVEL SECURITY;
DROP TRIGGER IF EXISTS prompt_request_dags_enforce_org ON prompt_request_dags;
DROP INDEX IF EXISTS prompt_request_dags_org_idx;
ALTER TABLE prompt_request_dags DROP COLUMN IF EXISTS org_id;

DROP POLICY IF EXISTS prompt_requests_org_isolation ON prompt_requests;
ALTER TABLE prompt_requests DISABLE ROW LEVEL SECURITY;
DROP TRIGGER IF EXISTS prompt_requests_enforce_org ON prompt_requests;
DROP INDEX IF EXISTS prompt_requests_pending_idx;
CREATE INDEX prompt_requests_pending_idx
    ON prompt_requests (session_id, created_at)
    WHERE status = 'pending';
ALTER TABLE prompt_requests
    DROP CONSTRAINT IF EXISTS prompt_requests_org_idempotency_key_key;
ALTER TABLE prompt_requests
    ADD CONSTRAINT prompt_requests_idempotency_key_key UNIQUE (idempotency_key);
ALTER TABLE prompt_requests DROP COLUMN IF EXISTS org_id;

DROP FUNCTION IF EXISTS enforce_runtime_row_parent_request_org();
DROP FUNCTION IF EXISTS enforce_runtime_row_parent_session_org();
