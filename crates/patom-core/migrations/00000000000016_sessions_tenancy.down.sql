-- Reverse migration 16. Drop triggers before functions before columns.

DROP POLICY IF EXISTS session_messages_org_isolation ON session_messages;
ALTER TABLE session_messages DISABLE ROW LEVEL SECURITY;
DROP TRIGGER IF EXISTS session_messages_enforce_org ON session_messages;
DROP FUNCTION IF EXISTS enforce_session_messages_org();
DROP INDEX IF EXISTS session_messages_org_idx;
ALTER TABLE session_messages DROP COLUMN IF EXISTS org_id;

DROP POLICY IF EXISTS sessions_org_isolation ON sessions;
ALTER TABLE sessions DISABLE ROW LEVEL SECURITY;
DROP TRIGGER IF EXISTS sessions_enforce_parent_org ON sessions;
DROP FUNCTION IF EXISTS enforce_sessions_parent_org();

DROP INDEX IF EXISTS sessions_root_idx;
CREATE INDEX sessions_root_idx ON sessions (root_request_id);

DROP INDEX IF EXISTS sessions_dag_pair_unique;
CREATE UNIQUE INDEX sessions_dag_pair_unique
    ON sessions (root_request_id,
                 participant_a_kind, participant_a_agent_id,
                 participant_b_kind, participant_b_agent_id)
    NULLS NOT DISTINCT;

ALTER TABLE sessions DROP COLUMN IF EXISTS created_by_user_id;
ALTER TABLE sessions DROP COLUMN IF EXISTS org_id;
