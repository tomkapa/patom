-- prompt_requests: sender/receiver flip from `(kind, agent_id)` pairs to a
-- single `colleague_id` FK per side. Same model as migration 58 for sessions
-- — the queue row addresses peers as colleagues, not bare satellites.
--
-- The receiver of a prompt_requests row is ALWAYS an agent (humans don't run
-- turns). The old CHECK `receiver_kind = 'agent'` is replaced by a trigger
-- that joins through `colleagues.kind` (a CHECK cannot reference another
-- table). Sender is `human` for HTTP/Slack/scheduler entry, `agent` for
-- librarian / reflection scheduler enqueues, and never `system` (System is
-- only ever a *session participant*, not a queue sender).

ALTER TABLE prompt_requests
    DROP CONSTRAINT IF EXISTS prompt_requests_sender_kind_agent,
    DROP CONSTRAINT IF EXISTS prompt_requests_receiver_kind_agent,
    DROP COLUMN sender_kind,
    DROP COLUMN sender_agent_id,
    DROP COLUMN receiver_kind,
    DROP COLUMN receiver_agent_id;

ALTER TABLE prompt_requests
    ADD COLUMN sender_colleague_id   UUID NOT NULL REFERENCES colleagues(id),
    ADD COLUMN receiver_colleague_id UUID NOT NULL REFERENCES colleagues(id);

-- Candidate-scan dispatch needs an index on `receiver_colleague_id` so the
-- "same agent across sessions" join in `claim_next_session` stays bounded.
CREATE INDEX prompt_requests_receiver_colleague_idx
    ON prompt_requests (receiver_colleague_id, status);

-- Enforce receiver-is-agent via a trigger. Same idiom as the org-parity
-- triggers from migration 18 — a CHECK can't reach across to
-- `colleagues.kind`, but a trigger can.
CREATE OR REPLACE FUNCTION enforce_prompt_receiver_is_agent() RETURNS TRIGGER
    LANGUAGE plpgsql AS $$
DECLARE
    receiver_kind TEXT;
BEGIN
    SELECT kind INTO receiver_kind
      FROM colleagues
     WHERE id = NEW.receiver_colleague_id;
    IF receiver_kind IS DISTINCT FROM 'agent' THEN
        RAISE EXCEPTION 'prompt_requests.receiver_colleague_id % is not an agent (kind=%)',
            NEW.receiver_colleague_id, receiver_kind;
    END IF;
    RETURN NEW;
END $$;

CREATE TRIGGER prompt_requests_receiver_agent
    BEFORE INSERT OR UPDATE OF receiver_colleague_id ON prompt_requests
    FOR EACH ROW EXECUTE FUNCTION enforce_prompt_receiver_is_agent();
