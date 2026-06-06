-- Reverse 00000000000004_multi_agent_comm.up.sql.

DROP TABLE IF EXISTS prompt_request_dags;

ALTER TABLE prompt_requests
    DROP CONSTRAINT IF EXISTS prompt_requests_sender_kind_agent,
    DROP COLUMN IF EXISTS root_request_id,
    DROP COLUMN IF EXISTS receiver_agent_id,
    DROP COLUMN IF EXISTS receiver_kind,
    DROP COLUMN IF EXISTS sender_agent_id,
    DROP COLUMN IF EXISTS sender_kind;

ALTER TABLE session_messages
    DROP CONSTRAINT IF EXISTS session_messages_receiver_kind_agent,
    DROP CONSTRAINT IF EXISTS session_messages_sender_kind_agent,
    DROP COLUMN IF EXISTS receiver_agent_id,
    DROP COLUMN IF EXISTS receiver_kind,
    DROP COLUMN IF EXISTS sender_agent_id,
    DROP COLUMN IF EXISTS sender_kind,
    ADD COLUMN role TEXT NOT NULL CHECK (role IN ('user','assistant'));

DROP INDEX IF EXISTS sessions_root_idx;
DROP INDEX IF EXISTS sessions_dag_pair_unique;

ALTER TABLE sessions
    DROP CONSTRAINT IF EXISTS sessions_participants_distinct,
    DROP CONSTRAINT IF EXISTS sessions_b_kind_agent,
    DROP CONSTRAINT IF EXISTS sessions_a_kind_agent,
    DROP COLUMN IF EXISTS participant_b_agent_id,
    DROP COLUMN IF EXISTS participant_b_kind,
    DROP COLUMN IF EXISTS participant_a_agent_id,
    DROP COLUMN IF EXISTS participant_a_kind,
    DROP COLUMN IF EXISTS root_request_id,
    DROP COLUMN IF EXISTS parent_session_id,
    ADD COLUMN agent_id UUID NOT NULL REFERENCES agents(id);
