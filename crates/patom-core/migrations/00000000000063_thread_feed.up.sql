-- Migration 63 — thread-feed chat model (the "thread-chat" refactor).
--
-- Replaces the 2-party pair-session model (sessions / session_messages) with a
-- Slack-exact channel -> thread -> flat-feed chat model. An agent's "session"
-- becomes (thread_id, agent_id) participation state; everyone else in a thread
-- is its counterparty. Reflection/resolution rehome to a private background-turn
-- store that is never part of any chat feed.
--
-- FULL RESET (friends-only prod, reset approved): no backfill. Existing runtime
-- rows are cleared up front so the new constraints apply on empty tables;
-- agent memories survive (their source_turn_id link is nulled, not cascaded).
-- See plan: thread-chat-agent-refactor.

-- ── Reset runtime state ──────────────────────────────────────────────────────
-- Cascades to session_messages, turn_metrics, tool_calls, prompt_request_dags,
-- reflection_checkpoints (last_turn_id); SET NULL on memory_events /
-- agent_memories source_turn_id (memories preserved).
DELETE FROM prompt_requests;

-- ── New chat tables ──────────────────────────────────────────────────────────

-- A thread is the per-thread ordering authority + canonical feed container.
-- channel_id NULL => DM. root_message_id => the channel-timeline message this
-- reply-thread hangs under (NULL for the channel's own timeline thread + DMs).
CREATE TABLE threads (
    id                       UUID PRIMARY KEY,
    org_id                   UUID NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    channel_id               UUID NULL,
    root_message_id          UUID NULL,
    created_by_colleague_id  UUID NOT NULL REFERENCES colleagues(id),
    created_at               TIMESTAMPTZ NOT NULL,
    last_activity_at         TIMESTAMPTZ NOT NULL,
    CONSTRAINT threads_channel_org_fk
        FOREIGN KEY (channel_id, org_id) REFERENCES channels (id, org_id) ON DELETE SET NULL (channel_id)
);
CREATE INDEX threads_channel_idx ON threads (org_id, channel_id, last_activity_at DESC);
CREATE INDEX threads_dm_idx
    ON threads (org_id, created_by_colleague_id, last_activity_at DESC) WHERE channel_id IS NULL;

-- The ONE canonical feed. `kind` discriminates posted chat (everyone's) from
-- per-agent private artifacts (owner_agent_id set). request_id is the producing
-- turn for agent rows; NULL for plain human posts.
CREATE TABLE thread_messages (
    id                    UUID NOT NULL DEFAULT gen_random_uuid(),
    thread_id             UUID NOT NULL REFERENCES threads(id) ON DELETE CASCADE,
    seq                   BIGINT NOT NULL,
    kind                  TEXT NOT NULL CHECK (kind IN ('posted','reasoning','tool_use','tool_result','system_note')),
    sender_colleague_id   UUID NULL REFERENCES colleagues(id),
    owner_agent_id        UUID NULL REFERENCES agents(id) ON DELETE CASCADE,
    receiver_colleague_id UUID NULL REFERENCES colleagues(id),
    body                  JSONB NOT NULL,
    request_id            UUID NULL REFERENCES prompt_requests(id) ON DELETE SET NULL,
    org_id                UUID NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    created_at            TIMESTAMPTZ NOT NULL,
    PRIMARY KEY (thread_id, seq),
    UNIQUE (id),
    CONSTRAINT thread_messages_owner_kind CHECK (
        (kind = 'posted' AND owner_agent_id IS NULL)
     OR (kind <> 'posted' AND owner_agent_id IS NOT NULL)),
    CONSTRAINT thread_messages_receiver_only_posted CHECK (
        receiver_colleague_id IS NULL OR kind = 'posted')
);
CREATE INDEX thread_messages_context_idx
    ON thread_messages (thread_id, seq) INCLUDE (kind, owner_agent_id);
CREATE INDEX thread_messages_feed_idx
    ON thread_messages (thread_id, created_at, seq);
-- A reply-thread's root is a channel-timeline message (lives in a different thread).
ALTER TABLE threads ADD CONSTRAINT threads_root_message_fk
    FOREIGN KEY (root_message_id) REFERENCES thread_messages (id) ON DELETE SET NULL;

-- Per-thread feed clock (replaces session_turn_seq for the message ordering).
CREATE TABLE thread_seq (
    thread_id UUID PRIMARY KEY REFERENCES threads(id) ON DELETE CASCADE,
    next_seq  BIGINT NOT NULL DEFAULT 0,
    org_id    UUID NOT NULL REFERENCES organizations(id) ON DELETE CASCADE
);

-- The redefined "session": an agent's participation in a thread.
CREATE TABLE agent_thread_state (
    id         UUID PRIMARY KEY,
    thread_id  UUID NOT NULL REFERENCES threads(id) ON DELETE CASCADE,
    agent_id   UUID NOT NULL REFERENCES agents(id) ON DELETE CASCADE,
    org_id     UUID NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    created_at TIMESTAMPTZ NOT NULL,
    UNIQUE (thread_id, agent_id)
);
CREATE INDEX agent_thread_state_thread_idx ON agent_thread_state (thread_id);

-- ── Private background cognition (reflection/resolution rehome) ───────────────

-- The rehome of the throwaway agent<->System pair-session. NEVER a chat row.
CREATE TABLE background_turns (
    id         UUID PRIMARY KEY,
    agent_id   UUID NOT NULL REFERENCES agents(id) ON DELETE CASCADE,
    org_id     UUID NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    created_at TIMESTAMPTZ NOT NULL
);
CREATE TABLE background_turn_messages (
    turn_id             UUID NOT NULL REFERENCES background_turns(id) ON DELETE CASCADE,
    seq                 BIGINT NOT NULL,
    sender_colleague_id UUID NULL REFERENCES colleagues(id),  -- NULL = System (injected prompt / tool results)
    body                JSONB NOT NULL,
    request_id          UUID NULL REFERENCES prompt_requests(id) ON DELETE SET NULL,
    org_id              UUID NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    created_at          TIMESTAMPTZ NOT NULL,
    PRIMARY KEY (turn_id, seq)
);

-- ── Unified claim lease (chat + cognition), keyed by polymorphic claim_key ────
-- claim_key = agent_thread_state.id (chat) OR background_turns.id (cognition).
-- Ephemeral runtime rows (reset_orphans reclaims them); no FK on claim_key.
CREATE TABLE claim_leases (
    claim_key    UUID PRIMARY KEY,
    worker_id    UUID NOT NULL,
    lease_seq    BIGINT NOT NULL,
    leased_until TIMESTAMPTZ NOT NULL,
    org_id       UUID NOT NULL REFERENCES organizations(id) ON DELETE CASCADE
);
CREATE TABLE claim_seq (
    claim_key UUID PRIMARY KEY,
    next_seq  BIGINT NOT NULL DEFAULT 0,
    org_id    UUID NOT NULL REFERENCES organizations(id) ON DELETE CASCADE
);

-- ── Org-parity triggers (pin a row's org_id to its parent's; CHECK can't) ─────
CREATE OR REPLACE FUNCTION enforce_thread_messages_org() RETURNS TRIGGER
    LANGUAGE plpgsql AS $$
DECLARE parent_org UUID;
BEGIN
    SELECT org_id INTO parent_org FROM threads WHERE id = NEW.thread_id;
    IF parent_org IS NULL THEN
        RAISE EXCEPTION 'thread_messages.thread_id % references missing thread', NEW.thread_id;
    END IF;
    IF parent_org <> NEW.org_id THEN
        RAISE EXCEPTION 'thread_messages.org_id % != thread % org %', NEW.org_id, NEW.thread_id, parent_org;
    END IF;
    RETURN NEW;
END $$;
CREATE TRIGGER thread_messages_enforce_org
    BEFORE INSERT OR UPDATE OF org_id, thread_id ON thread_messages
    FOR EACH ROW EXECUTE FUNCTION enforce_thread_messages_org();

CREATE OR REPLACE FUNCTION enforce_agent_thread_state_org() RETURNS TRIGGER
    LANGUAGE plpgsql AS $$
DECLARE parent_org UUID;
BEGIN
    SELECT org_id INTO parent_org FROM threads WHERE id = NEW.thread_id;
    IF parent_org IS NULL THEN
        RAISE EXCEPTION 'agent_thread_state.thread_id % references missing thread', NEW.thread_id;
    END IF;
    IF parent_org <> NEW.org_id THEN
        RAISE EXCEPTION 'agent_thread_state.org_id % != thread % org %', NEW.org_id, NEW.thread_id, parent_org;
    END IF;
    RETURN NEW;
END $$;
CREATE TRIGGER agent_thread_state_enforce_org
    BEFORE INSERT OR UPDATE OF org_id, thread_id ON agent_thread_state
    FOR EACH ROW EXECUTE FUNCTION enforce_agent_thread_state_org();

-- ── RLS: org isolation on every new table (defense-in-depth; channel-membership
--    is the load-bearing gate in the query layer) ─────────────────────────────
ALTER TABLE threads ENABLE ROW LEVEL SECURITY;
ALTER TABLE threads FORCE ROW LEVEL SECURITY;
CREATE POLICY threads_org_isolation ON threads FOR ALL TO PUBLIC
    USING (app_user_is_member(org_id)) WITH CHECK (app_user_is_member(org_id));

ALTER TABLE thread_messages ENABLE ROW LEVEL SECURITY;
ALTER TABLE thread_messages FORCE ROW LEVEL SECURITY;
CREATE POLICY thread_messages_org_isolation ON thread_messages FOR ALL TO PUBLIC
    USING (app_user_is_member(org_id)) WITH CHECK (app_user_is_member(org_id));

ALTER TABLE thread_seq ENABLE ROW LEVEL SECURITY;
ALTER TABLE thread_seq FORCE ROW LEVEL SECURITY;
CREATE POLICY thread_seq_org_isolation ON thread_seq FOR ALL TO PUBLIC
    USING (app_user_is_member(org_id)) WITH CHECK (app_user_is_member(org_id));

ALTER TABLE agent_thread_state ENABLE ROW LEVEL SECURITY;
ALTER TABLE agent_thread_state FORCE ROW LEVEL SECURITY;
CREATE POLICY agent_thread_state_org_isolation ON agent_thread_state FOR ALL TO PUBLIC
    USING (app_user_is_member(org_id)) WITH CHECK (app_user_is_member(org_id));

ALTER TABLE background_turns ENABLE ROW LEVEL SECURITY;
ALTER TABLE background_turns FORCE ROW LEVEL SECURITY;
CREATE POLICY background_turns_org_isolation ON background_turns FOR ALL TO PUBLIC
    USING (app_user_is_member(org_id)) WITH CHECK (app_user_is_member(org_id));

ALTER TABLE background_turn_messages ENABLE ROW LEVEL SECURITY;
ALTER TABLE background_turn_messages FORCE ROW LEVEL SECURITY;
CREATE POLICY background_turn_messages_org_isolation ON background_turn_messages FOR ALL TO PUBLIC
    USING (app_user_is_member(org_id)) WITH CHECK (app_user_is_member(org_id));

ALTER TABLE claim_leases ENABLE ROW LEVEL SECURITY;
ALTER TABLE claim_leases FORCE ROW LEVEL SECURITY;
CREATE POLICY claim_leases_org_isolation ON claim_leases FOR ALL TO PUBLIC
    USING (app_user_is_member(org_id)) WITH CHECK (app_user_is_member(org_id));

ALTER TABLE claim_seq ENABLE ROW LEVEL SECURITY;
ALTER TABLE claim_seq FORCE ROW LEVEL SECURITY;
CREATE POLICY claim_seq_org_isolation ON claim_seq FOR ALL TO PUBLIC
    USING (app_user_is_member(org_id)) WITH CHECK (app_user_is_member(org_id));

-- ── prompt_requests becomes a trigger row ────────────────────────────────────
ALTER TABLE prompt_requests
    ADD COLUMN thread_id          UUID NULL REFERENCES threads(id) ON DELETE CASCADE,
    ADD COLUMN state_id           UUID NULL REFERENCES agent_thread_state(id) ON DELETE CASCADE,
    ADD COLUMN background_turn_id UUID NULL REFERENCES background_turns(id) ON DELETE CASCADE,
    ADD COLUMN trigger_message_id UUID NULL REFERENCES thread_messages(id) ON DELETE SET NULL,
    ADD COLUMN acting_user_id     UUID NULL REFERENCES users(id),
    ADD CONSTRAINT prompt_requests_claim_key_xor CHECK (
        (state_id IS NOT NULL) <> (background_turn_id IS NOT NULL));
ALTER TABLE prompt_requests ALTER COLUMN content DROP NOT NULL;  -- vestigial; chat msg lives in thread_messages
-- The old org-parity trigger reads session_id + joins sessions, and the pending
-- index leads with session_id — drop both before dropping the column they depend
-- on. A thread/state-scoped parity trigger returns when the queue is re-keyed (P2);
-- RLS still gates org membership in the interim.
DROP TRIGGER prompt_requests_enforce_org ON prompt_requests;
DROP INDEX prompt_requests_pending_idx;
ALTER TABLE prompt_requests DROP COLUMN session_id;             -- sessions dropped below
CREATE INDEX prompt_requests_pending_idx
    ON prompt_requests (org_id, COALESCE(state_id, background_turn_id), created_at) WHERE status = 'pending';

-- ── Scheduling gains a target channel ────────────────────────────────────────
ALTER TABLE scheduled_tasks ADD COLUMN channel_id UUID NULL;

-- ── Repoint FK-dependents off sessions (empty after the reset) ────────────────

-- turn_metrics: session_id -> state_id.
DROP TRIGGER turn_metrics_enforce_org ON turn_metrics;
DROP INDEX turn_metrics_session_idx;
ALTER TABLE turn_metrics DROP COLUMN session_id;
ALTER TABLE turn_metrics ADD COLUMN state_id UUID NOT NULL REFERENCES agent_thread_state(id) ON DELETE CASCADE;
CREATE INDEX turn_metrics_state_idx ON turn_metrics (state_id, started_at DESC);
CREATE OR REPLACE FUNCTION enforce_turn_metrics_org() RETURNS TRIGGER
    LANGUAGE plpgsql AS $$
DECLARE parent_org UUID;
BEGIN
    SELECT org_id INTO parent_org FROM agent_thread_state WHERE id = NEW.state_id;
    IF parent_org IS NULL THEN
        RAISE EXCEPTION 'turn_metrics.state_id % references missing state', NEW.state_id;
    END IF;
    IF parent_org <> NEW.org_id THEN
        RAISE EXCEPTION 'turn_metrics.org_id % != state % org %', NEW.org_id, NEW.state_id, parent_org;
    END IF;
    RETURN NEW;
END $$;
CREATE TRIGGER turn_metrics_enforce_org
    BEFORE INSERT OR UPDATE OF org_id, state_id ON turn_metrics
    FOR EACH ROW EXECUTE FUNCTION enforce_turn_metrics_org();

-- tool_calls: session_id -> state_id.
DROP TRIGGER tool_calls_enforce_org ON tool_calls;
ALTER TABLE tool_calls DROP COLUMN session_id;
ALTER TABLE tool_calls ADD COLUMN state_id UUID NOT NULL REFERENCES agent_thread_state(id) ON DELETE CASCADE;
CREATE OR REPLACE FUNCTION enforce_tool_calls_org() RETURNS TRIGGER
    LANGUAGE plpgsql AS $$
DECLARE parent_org UUID;
BEGIN
    SELECT org_id INTO parent_org FROM agent_thread_state WHERE id = NEW.state_id;
    IF parent_org IS NULL THEN
        RAISE EXCEPTION 'tool_calls.state_id % references missing state', NEW.state_id;
    END IF;
    IF parent_org <> NEW.org_id THEN
        RAISE EXCEPTION 'tool_calls.org_id % != state % org %', NEW.org_id, NEW.state_id, parent_org;
    END IF;
    RETURN NEW;
END $$;
CREATE TRIGGER tool_calls_enforce_org
    BEFORE INSERT OR UPDATE OF org_id, state_id ON tool_calls
    FOR EACH ROW EXECUTE FUNCTION enforce_tool_calls_org();

-- session_todos: PK was session_id -> recreate keyed by state_id.
DROP TABLE session_todos;
CREATE TABLE session_todos (
    state_id              UUID        PRIMARY KEY REFERENCES agent_thread_state(id) ON DELETE CASCADE,
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
CREATE OR REPLACE FUNCTION enforce_session_todos_parent_org() RETURNS TRIGGER
    LANGUAGE plpgsql AS $$
DECLARE parent_org UUID;
BEGIN
    SELECT org_id INTO parent_org FROM agent_thread_state WHERE id = NEW.state_id;
    IF parent_org IS NULL THEN
        RAISE EXCEPTION 'session_todos.state_id % references missing state', NEW.state_id;
    END IF;
    IF parent_org <> NEW.org_id THEN
        RAISE EXCEPTION 'session_todos.org_id % != state % org %', NEW.org_id, NEW.state_id, parent_org;
    END IF;
    RETURN NEW;
END $$;
CREATE TRIGGER session_todos_enforce_org
    BEFORE INSERT OR UPDATE OF org_id, state_id ON session_todos
    FOR EACH ROW EXECUTE FUNCTION enforce_session_todos_parent_org();
ALTER TABLE session_todos ENABLE ROW LEVEL SECURITY;
ALTER TABLE session_todos FORCE ROW LEVEL SECURITY;
CREATE POLICY session_todos_org_isolation ON session_todos FOR ALL TO PUBLIC
    USING (app_user_is_member(org_id)) WITH CHECK (app_user_is_member(org_id));

-- reflection_checkpoints: PK was (agent_id, session_id) -> (agent_id, thread_id).
DROP TABLE reflection_checkpoints;
CREATE TABLE reflection_checkpoints (
    agent_id              UUID NOT NULL REFERENCES agents(id) ON DELETE CASCADE,
    thread_id             UUID NOT NULL REFERENCES threads(id) ON DELETE CASCADE,
    last_message_id       UUID NOT NULL REFERENCES thread_messages(id) ON DELETE CASCADE,
    reflection_event_id   UUID NULL REFERENCES memory_events(id) ON DELETE SET NULL,
    org_id                UUID NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    created_at            TIMESTAMPTZ NOT NULL,
    PRIMARY KEY (agent_id, thread_id)
);
CREATE INDEX reflection_checkpoints_org_idx ON reflection_checkpoints (org_id);
ALTER TABLE reflection_checkpoints ENABLE ROW LEVEL SECURITY;
ALTER TABLE reflection_checkpoints FORCE ROW LEVEL SECURITY;
CREATE POLICY reflection_checkpoints_org_isolation ON reflection_checkpoints FOR ALL TO PUBLIC
    USING (app_user_is_member(org_id)) WITH CHECK (app_user_is_member(org_id));

-- ── Drop the pair-session model ──────────────────────────────────────────────
DROP TABLE session_messages;
DROP TABLE session_turn_seq;
DROP TABLE session_leases;
DROP TABLE sessions;
