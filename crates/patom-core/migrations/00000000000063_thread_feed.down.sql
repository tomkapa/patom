-- Down for migration 63 — LOSSY structure-only reverse (full reset was approved;
-- no data is restored, and the org-parity triggers on the recreated session
-- tables are intentionally not reproduced). Restores enough schema for
-- `sqlx migrate revert` to leave a consistent, applies-able state.

-- ── Recreate the pair-session model ──────────────────────────────────────────
CREATE TABLE sessions (
    id                         UUID PRIMARY KEY,
    created_at                 TIMESTAMPTZ NOT NULL,
    parent_session_id          UUID NULL REFERENCES sessions(id) ON DELETE SET NULL,
    root_request_id            UUID NOT NULL,
    participant_a_colleague_id UUID NOT NULL REFERENCES colleagues(id),
    participant_b_colleague_id UUID NULL REFERENCES colleagues(id),
    org_id                     UUID NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    created_by_user_id         UUID NOT NULL REFERENCES users(id),
    CONSTRAINT sessions_participants_distinct CHECK (
        participant_b_colleague_id IS NULL
        OR participant_a_colleague_id < participant_b_colleague_id)
);
CREATE UNIQUE INDEX sessions_dag_pair_unique
    ON sessions (org_id, root_request_id, participant_a_colleague_id, participant_b_colleague_id)
    NULLS NOT DISTINCT;
CREATE INDEX sessions_root_idx ON sessions (org_id, root_request_id);
ALTER TABLE sessions ENABLE ROW LEVEL SECURITY;
ALTER TABLE sessions FORCE ROW LEVEL SECURITY;
CREATE POLICY sessions_org_isolation ON sessions FOR ALL TO PUBLIC
    USING (app_user_is_member(org_id)) WITH CHECK (app_user_is_member(org_id));

CREATE TABLE session_messages (
    session_id            UUID NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    seq                   BIGINT NOT NULL,
    request_id            UUID NOT NULL REFERENCES prompt_requests(id) ON DELETE CASCADE,
    sender_colleague_id   UUID NULL REFERENCES colleagues(id),
    receiver_colleague_id UUID NOT NULL REFERENCES colleagues(id),
    body                  JSONB NOT NULL,
    created_at            TIMESTAMPTZ NOT NULL,
    org_id                UUID NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    PRIMARY KEY (session_id, seq)
);
CREATE INDEX session_messages_request_id_idx ON session_messages (request_id);
CREATE INDEX session_messages_org_idx ON session_messages (org_id);
ALTER TABLE session_messages ENABLE ROW LEVEL SECURITY;
ALTER TABLE session_messages FORCE ROW LEVEL SECURITY;
CREATE POLICY session_messages_org_isolation ON session_messages FOR ALL TO PUBLIC
    USING (app_user_is_member(org_id)) WITH CHECK (app_user_is_member(org_id));

CREATE TABLE session_turn_seq (
    session_id UUID PRIMARY KEY REFERENCES sessions(id) ON DELETE CASCADE,
    next_seq   BIGINT NOT NULL DEFAULT 0,
    org_id     UUID NOT NULL REFERENCES organizations(id) ON DELETE CASCADE
);
CREATE INDEX session_turn_seq_org_idx ON session_turn_seq (org_id);
ALTER TABLE session_turn_seq ENABLE ROW LEVEL SECURITY;
ALTER TABLE session_turn_seq FORCE ROW LEVEL SECURITY;
CREATE POLICY session_turn_seq_org_isolation ON session_turn_seq FOR ALL TO PUBLIC
    USING (app_user_is_member(org_id)) WITH CHECK (app_user_is_member(org_id));

CREATE TABLE session_leases (
    session_id   UUID PRIMARY KEY REFERENCES sessions(id) ON DELETE CASCADE,
    worker_id    UUID NOT NULL,
    turn_seq     BIGINT NOT NULL,
    leased_until TIMESTAMPTZ NOT NULL,
    org_id       UUID NOT NULL REFERENCES organizations(id) ON DELETE CASCADE
);
CREATE INDEX session_leases_org_idx ON session_leases (org_id);
ALTER TABLE session_leases ENABLE ROW LEVEL SECURITY;
ALTER TABLE session_leases FORCE ROW LEVEL SECURITY;
CREATE POLICY session_leases_org_isolation ON session_leases FOR ALL TO PUBLIC
    USING (app_user_is_member(org_id)) WITH CHECK (app_user_is_member(org_id));

-- ── Revert reflection_checkpoints to (agent_id, session_id) ───────────────────
DROP TABLE reflection_checkpoints;
CREATE TABLE reflection_checkpoints (
    agent_id              UUID NOT NULL REFERENCES agents(id) ON DELETE CASCADE,
    session_id            UUID NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    last_turn_id          UUID NOT NULL REFERENCES prompt_requests(id) ON DELETE CASCADE,
    reflection_event_id   UUID NULL REFERENCES memory_events(id) ON DELETE SET NULL,
    reflection_session_ids UUID[] NOT NULL DEFAULT '{}',
    org_id                UUID NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    created_at            TIMESTAMPTZ NOT NULL,
    PRIMARY KEY (agent_id, session_id)
);
CREATE INDEX reflection_checkpoints_org_idx ON reflection_checkpoints (org_id);
ALTER TABLE reflection_checkpoints ENABLE ROW LEVEL SECURITY;
ALTER TABLE reflection_checkpoints FORCE ROW LEVEL SECURITY;
CREATE POLICY reflection_checkpoints_org_isolation ON reflection_checkpoints FOR ALL TO PUBLIC
    USING (app_user_is_member(org_id)) WITH CHECK (app_user_is_member(org_id));

-- ── Revert session_todos to session_id PK ────────────────────────────────────
DROP TABLE session_todos;
CREATE TABLE session_todos (
    session_id            UUID        PRIMARY KEY REFERENCES sessions(id) ON DELETE CASCADE,
    org_id                UUID        NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    items                 JSONB       NOT NULL,
    item_count            SMALLINT    NOT NULL,
    updated_at            TIMESTAMPTZ NOT NULL,
    updated_in_request_id UUID        NOT NULL,
    CONSTRAINT session_todos_items_well_formed CHECK (
        jsonb_typeof(items) = 'array'
        AND item_count BETWEEN 0 AND 50
        AND item_count = jsonb_array_length(items))
);
CREATE INDEX session_todos_org_idx ON session_todos (org_id);
ALTER TABLE session_todos ENABLE ROW LEVEL SECURITY;
ALTER TABLE session_todos FORCE ROW LEVEL SECURITY;
CREATE POLICY session_todos_org_isolation ON session_todos FOR ALL TO PUBLIC
    USING (app_user_is_member(org_id)) WITH CHECK (app_user_is_member(org_id));

-- ── Revert tool_calls: state_id -> session_id ────────────────────────────────
DROP TRIGGER tool_calls_enforce_org ON tool_calls;
ALTER TABLE tool_calls DROP COLUMN state_id;
ALTER TABLE tool_calls ADD COLUMN session_id UUID NOT NULL REFERENCES sessions(id) ON DELETE CASCADE;
CREATE OR REPLACE FUNCTION enforce_tool_calls_org() RETURNS TRIGGER
    LANGUAGE plpgsql AS $$
DECLARE parent_org UUID;
BEGIN
    SELECT org_id INTO parent_org FROM sessions WHERE id = NEW.session_id;
    IF parent_org IS NULL THEN
        RAISE EXCEPTION 'tool_calls.session_id % references missing session', NEW.session_id;
    END IF;
    IF parent_org <> NEW.org_id THEN
        RAISE EXCEPTION 'tool_calls.org_id % does not match parent session % org %', NEW.org_id, NEW.session_id, parent_org;
    END IF;
    RETURN NEW;
END $$;
CREATE TRIGGER tool_calls_enforce_org
    BEFORE INSERT OR UPDATE OF org_id, session_id ON tool_calls
    FOR EACH ROW EXECUTE FUNCTION enforce_tool_calls_org();

-- ── Revert turn_metrics: state_id -> session_id ──────────────────────────────
DROP TRIGGER turn_metrics_enforce_org ON turn_metrics;
DROP INDEX turn_metrics_state_idx;
ALTER TABLE turn_metrics DROP COLUMN state_id;
ALTER TABLE turn_metrics ADD COLUMN session_id UUID NOT NULL REFERENCES sessions(id) ON DELETE CASCADE;
CREATE INDEX turn_metrics_session_idx ON turn_metrics (session_id, started_at DESC);
CREATE OR REPLACE FUNCTION enforce_turn_metrics_org() RETURNS TRIGGER
    LANGUAGE plpgsql AS $$
DECLARE parent_org UUID;
BEGIN
    SELECT org_id INTO parent_org FROM sessions WHERE id = NEW.session_id;
    IF parent_org IS NULL THEN
        RAISE EXCEPTION 'turn_metrics.session_id % references missing session', NEW.session_id;
    END IF;
    IF parent_org <> NEW.org_id THEN
        RAISE EXCEPTION 'turn_metrics.org_id % does not match parent session % org %', NEW.org_id, NEW.session_id, parent_org;
    END IF;
    RETURN NEW;
END $$;
CREATE TRIGGER turn_metrics_enforce_org
    BEFORE INSERT OR UPDATE OF org_id, session_id ON turn_metrics
    FOR EACH ROW EXECUTE FUNCTION enforce_turn_metrics_org();

-- ── Revert scheduling ────────────────────────────────────────────────────────
ALTER TABLE scheduled_tasks DROP COLUMN channel_id;

-- ── Revert prompt_requests ───────────────────────────────────────────────────
DROP INDEX prompt_requests_pending_idx;
ALTER TABLE prompt_requests DROP CONSTRAINT prompt_requests_claim_key_xor;
ALTER TABLE prompt_requests
    DROP COLUMN thread_id,
    DROP COLUMN state_id,
    DROP COLUMN background_turn_id,
    DROP COLUMN trigger_message_id,
    DROP COLUMN acting_user_id;
ALTER TABLE prompt_requests ADD COLUMN session_id UUID NOT NULL REFERENCES sessions(id) ON DELETE CASCADE;
ALTER TABLE prompt_requests ALTER COLUMN content SET NOT NULL;
CREATE INDEX prompt_requests_pending_idx
    ON prompt_requests (org_id, session_id, created_at) WHERE status = 'pending';
CREATE TRIGGER prompt_requests_enforce_org
    BEFORE INSERT OR UPDATE OF org_id, session_id ON prompt_requests
    FOR EACH ROW EXECUTE FUNCTION enforce_runtime_row_parent_session_org();

-- ── Drop the new feed model ──────────────────────────────────────────────────
DROP TABLE claim_seq;
DROP TABLE claim_leases;
DROP TABLE background_turn_messages;
DROP TABLE background_turns;
DROP TRIGGER agent_thread_state_enforce_org ON agent_thread_state;
DROP TABLE agent_thread_state;
DROP TABLE thread_seq;
ALTER TABLE threads DROP CONSTRAINT threads_root_message_fk;
DROP TRIGGER thread_messages_enforce_org ON thread_messages;
DROP TABLE thread_messages;
DROP TABLE threads;
DROP FUNCTION IF EXISTS enforce_thread_messages_org();
DROP FUNCTION IF EXISTS enforce_agent_thread_state_org();
