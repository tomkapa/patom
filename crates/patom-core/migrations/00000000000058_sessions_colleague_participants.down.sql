-- Reverse of migration 58. Reset-allowed: no data preservation, just shape.
ALTER TABLE session_messages
    DROP COLUMN sender_colleague_id,
    DROP COLUMN receiver_colleague_id;

ALTER TABLE session_messages
    ADD COLUMN sender_kind       TEXT NOT NULL
                                 CHECK (sender_kind IN ('human','agent','system')),
    ADD COLUMN sender_agent_id   UUID NULL REFERENCES agents(id),
    ADD COLUMN receiver_kind     TEXT NOT NULL
                                 CHECK (receiver_kind IN ('human','agent')),
    ADD COLUMN receiver_agent_id UUID NULL REFERENCES agents(id),
    ADD CONSTRAINT session_messages_sender_kind_agent CHECK (
        (sender_kind = 'agent') = (sender_agent_id IS NOT NULL)
    ),
    ADD CONSTRAINT session_messages_receiver_kind_agent CHECK (
        (receiver_kind = 'agent') = (receiver_agent_id IS NOT NULL)
    );

DROP INDEX IF EXISTS sessions_dag_pair_unique;
ALTER TABLE sessions
    DROP CONSTRAINT IF EXISTS sessions_participants_distinct,
    DROP COLUMN participant_a_colleague_id,
    DROP COLUMN participant_b_colleague_id;

ALTER TABLE sessions
    ADD COLUMN participant_a_kind     TEXT NOT NULL
                                      CHECK (participant_a_kind IN ('human','agent')),
    ADD COLUMN participant_a_agent_id UUID NULL REFERENCES agents(id),
    ADD COLUMN participant_b_kind     TEXT NOT NULL
                                      CHECK (participant_b_kind IN ('human','agent')),
    ADD COLUMN participant_b_agent_id UUID NULL REFERENCES agents(id),
    ADD CONSTRAINT sessions_a_kind_agent CHECK (
        (participant_a_kind = 'agent') = (participant_a_agent_id IS NOT NULL)
    ),
    ADD CONSTRAINT sessions_b_kind_agent CHECK (
        (participant_b_kind = 'agent') = (participant_b_agent_id IS NOT NULL)
    ),
    ADD CONSTRAINT sessions_participants_distinct CHECK (
        (participant_a_kind, participant_a_agent_id)
        < (participant_b_kind, participant_b_agent_id)
    );

CREATE UNIQUE INDEX sessions_dag_pair_unique
    ON sessions (org_id, root_request_id,
                 participant_a_kind, participant_a_agent_id,
                 participant_b_kind, participant_b_agent_id)
    NULLS NOT DISTINCT;
