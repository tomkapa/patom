-- Reverse of migration 60. Reset-allowed: no data preservation.
DROP TRIGGER IF EXISTS prompt_requests_receiver_agent ON prompt_requests;
DROP FUNCTION IF EXISTS enforce_prompt_receiver_is_agent();
DROP INDEX IF EXISTS prompt_requests_receiver_colleague_idx;

ALTER TABLE prompt_requests
    DROP COLUMN sender_colleague_id,
    DROP COLUMN receiver_colleague_id;

ALTER TABLE prompt_requests
    ADD COLUMN sender_kind       TEXT NOT NULL
                                 CHECK (sender_kind IN ('human','agent')),
    ADD COLUMN sender_agent_id   UUID NULL REFERENCES agents(id),
    ADD COLUMN receiver_kind     TEXT NOT NULL
                                 CHECK (receiver_kind = 'agent'),
    ADD COLUMN receiver_agent_id UUID NOT NULL REFERENCES agents(id),
    ADD CONSTRAINT prompt_requests_sender_kind_agent CHECK (
        (sender_kind = 'agent') = (sender_agent_id IS NOT NULL)
    );
