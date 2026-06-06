-- Memory tenancy retrofit. Follows the `mcp_servers` (migration 14),
-- `agents` (15), and `sessions` (16) pattern: each of the five memory
-- tables gains `org_id`, a BEFORE-trigger that pins the column to the
-- parent agent's org (a CHECK can't reference another row), ENABLE +
-- FORCE row level security, and one `<tbl>_org_isolation` policy keyed
-- on `app_user_is_member(org_id)`.
--
-- Tables retrofitted (all hang off `agents`, which is now per-org):
--
--   * memory_events            — append-only journal
--   * agent_memories           — materialised view
--   * contradiction_events     — librarian-detected pairs
--   * reflection_checkpoints   — per (agent, session) reflection cursor
--   * validation_events        — independent-signal log
--
-- Denormalising `org_id` onto every row keeps the RLS predicate
-- self-contained (no JOIN through `agents` in the policy hot path) and
-- mirrors what `session_messages` did in migration 16. The parity
-- triggers enforce that the denormalised column always agrees with the
-- parent agent's org so raw SQL cannot smuggle a cross-org row in.
--
-- Pre-launch: NOT NULL with no backfill (see `feedback_no_backcompat`).
-- Existing dev rows must be wiped before applying; the test schema is
-- minted fresh per test so the test path is naturally unaffected.

-- ───────────────────────────────────────────────────────────────────────────
-- Shared parity-trigger function. Reads the parent agent's `org_id` and
-- raises if the row's `org_id` does not match. Used by all five tables —
-- one function, five triggers — because the predicate is identical and
-- the column name (`agent_id`) is the same on every table. The trigger
-- is registered BEFORE INSERT OR UPDATE OF the columns that could
-- invalidate the parity (`org_id`, `agent_id`).
-- ───────────────────────────────────────────────────────────────────────────
CREATE OR REPLACE FUNCTION enforce_memory_row_parent_org() RETURNS TRIGGER
    LANGUAGE plpgsql AS $$
DECLARE
    parent_org UUID;
BEGIN
    SELECT org_id INTO parent_org FROM agents WHERE id = NEW.agent_id;
    IF parent_org IS NULL THEN
        RAISE EXCEPTION
            '%.agent_id % references missing agent',
            TG_TABLE_NAME, NEW.agent_id;
    END IF;
    IF parent_org <> NEW.org_id THEN
        RAISE EXCEPTION
            '%.org_id % does not match parent agent % org %',
            TG_TABLE_NAME, NEW.org_id, NEW.agent_id, parent_org;
    END IF;
    RETURN NEW;
END
$$;

-- ───────────────────────────────────────────────────────────────────────────
-- memory_events
-- ───────────────────────────────────────────────────────────────────────────

ALTER TABLE memory_events
    ADD COLUMN org_id UUID NOT NULL REFERENCES organizations(id) ON DELETE CASCADE;

CREATE INDEX memory_events_org_idx ON memory_events (org_id);

CREATE TRIGGER memory_events_enforce_org
    BEFORE INSERT OR UPDATE OF org_id, agent_id ON memory_events
    FOR EACH ROW
    EXECUTE FUNCTION enforce_memory_row_parent_org();

ALTER TABLE memory_events ENABLE ROW LEVEL SECURITY;
ALTER TABLE memory_events FORCE ROW LEVEL SECURITY;
CREATE POLICY memory_events_org_isolation ON memory_events
    FOR ALL TO PUBLIC
    USING      (app_user_is_member(org_id))
    WITH CHECK (app_user_is_member(org_id));

-- ───────────────────────────────────────────────────────────────────────────
-- agent_memories
-- ───────────────────────────────────────────────────────────────────────────

ALTER TABLE agent_memories
    ADD COLUMN org_id UUID NOT NULL REFERENCES organizations(id) ON DELETE CASCADE;

CREATE INDEX agent_memories_org_idx ON agent_memories (org_id);

CREATE TRIGGER agent_memories_enforce_org
    BEFORE INSERT OR UPDATE OF org_id, agent_id ON agent_memories
    FOR EACH ROW
    EXECUTE FUNCTION enforce_memory_row_parent_org();

ALTER TABLE agent_memories ENABLE ROW LEVEL SECURITY;
ALTER TABLE agent_memories FORCE ROW LEVEL SECURITY;
CREATE POLICY agent_memories_org_isolation ON agent_memories
    FOR ALL TO PUBLIC
    USING      (app_user_is_member(org_id))
    WITH CHECK (app_user_is_member(org_id));

-- ───────────────────────────────────────────────────────────────────────────
-- contradiction_events
-- ───────────────────────────────────────────────────────────────────────────

ALTER TABLE contradiction_events
    ADD COLUMN org_id UUID NOT NULL REFERENCES organizations(id) ON DELETE CASCADE;

CREATE INDEX contradiction_events_org_idx ON contradiction_events (org_id);

CREATE TRIGGER contradiction_events_enforce_org
    BEFORE INSERT OR UPDATE OF org_id, agent_id ON contradiction_events
    FOR EACH ROW
    EXECUTE FUNCTION enforce_memory_row_parent_org();

ALTER TABLE contradiction_events ENABLE ROW LEVEL SECURITY;
ALTER TABLE contradiction_events FORCE ROW LEVEL SECURITY;
CREATE POLICY contradiction_events_org_isolation ON contradiction_events
    FOR ALL TO PUBLIC
    USING      (app_user_is_member(org_id))
    WITH CHECK (app_user_is_member(org_id));

-- ───────────────────────────────────────────────────────────────────────────
-- reflection_checkpoints
-- ───────────────────────────────────────────────────────────────────────────

ALTER TABLE reflection_checkpoints
    ADD COLUMN org_id UUID NOT NULL REFERENCES organizations(id) ON DELETE CASCADE;

CREATE INDEX reflection_checkpoints_org_idx ON reflection_checkpoints (org_id);

CREATE TRIGGER reflection_checkpoints_enforce_org
    BEFORE INSERT OR UPDATE OF org_id, agent_id ON reflection_checkpoints
    FOR EACH ROW
    EXECUTE FUNCTION enforce_memory_row_parent_org();

ALTER TABLE reflection_checkpoints ENABLE ROW LEVEL SECURITY;
ALTER TABLE reflection_checkpoints FORCE ROW LEVEL SECURITY;
CREATE POLICY reflection_checkpoints_org_isolation ON reflection_checkpoints
    FOR ALL TO PUBLIC
    USING      (app_user_is_member(org_id))
    WITH CHECK (app_user_is_member(org_id));

-- ───────────────────────────────────────────────────────────────────────────
-- validation_events
-- ───────────────────────────────────────────────────────────────────────────

ALTER TABLE validation_events
    ADD COLUMN org_id UUID NOT NULL REFERENCES organizations(id) ON DELETE CASCADE;

CREATE INDEX validation_events_org_idx ON validation_events (org_id);

-- The parity trigger pins against `agent_id`. `memory_id` is structurally
-- redundant: `agent_memories` is itself parity-pinned to its agent's org,
-- so `validation_events.memory_id`'s implicit org is already equal to
-- `validation_events.agent_id`'s implicit org. One check is enough.
CREATE TRIGGER validation_events_enforce_org
    BEFORE INSERT OR UPDATE OF org_id, agent_id ON validation_events
    FOR EACH ROW
    EXECUTE FUNCTION enforce_memory_row_parent_org();

ALTER TABLE validation_events ENABLE ROW LEVEL SECURITY;
ALTER TABLE validation_events FORCE ROW LEVEL SECURITY;
CREATE POLICY validation_events_org_isolation ON validation_events
    FOR ALL TO PUBLIC
    USING      (app_user_is_member(org_id))
    WITH CHECK (app_user_is_member(org_id));
