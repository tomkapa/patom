-- agent_prompt_versions — append-only history of an agent's system_prompt.
--
-- Versions ONLY the system_prompt. The model lives on `agents.model` and
-- is mutated in place — a model swap is a runtime preference (which
-- backend serves this agent), independent of the prompt that defines
-- the agent's behaviour. Coupling them here would falsely conflate
-- "the agent's voice changed" with "we routed to a different provider";
-- the Logs & Metrics tab still records both per-turn dimensions on
-- `turn_metrics` so analytics can group on either axis.
--
-- "Current" is `MAX(version) WHERE agent_id = X` — derivable from the
-- UNIQUE index below via a backwards index scan. Restore is append-only
-- (every revert mints a fresh `max+1` row), so this holds for every
-- mutation path; no `current_prompt_version_id` pointer column.
--
-- Pre-launch single-step migration: NOT NULL with no backfill. Dev DBs are
-- wiped before applying (feedback_no_backcompat).

CREATE TABLE agent_prompt_versions (
    id            UUID PRIMARY KEY,
    agent_id      UUID NOT NULL REFERENCES agents(id) ON DELETE CASCADE,
    org_id        UUID NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    version       INTEGER NOT NULL CHECK (version > 0),
    system_prompt TEXT NOT NULL,
    -- NULL = system seed (e.g. the per-org default agent created at sign-up)
    -- or any path where no user principal was in hand.
    edited_by     UUID,
    created_at    TIMESTAMPTZ NOT NULL,
    UNIQUE (agent_id, version)
);

-- Timeline queries lead with (agent_id, created_at DESC) — same shape as
-- `tool_calls_per_agent_mcp_idx` in migration 25.
CREATE INDEX agent_prompt_versions_agent_idx
    ON agent_prompt_versions (agent_id, created_at DESC);

-- Denormalised org_id must match the parent agent's. Trigger mirrors
-- enforce_tool_calls_org in migration 25; CHECK constraints can't reference
-- other rows so the trigger is the load-bearing piece.
CREATE OR REPLACE FUNCTION enforce_agent_prompt_versions_org() RETURNS TRIGGER
    LANGUAGE plpgsql AS $$
DECLARE
    parent_org UUID;
BEGIN
    SELECT org_id INTO parent_org FROM agents WHERE id = NEW.agent_id;
    IF parent_org IS NULL THEN
        RAISE EXCEPTION
            'agent_prompt_versions.agent_id % references missing agent',
            NEW.agent_id;
    END IF;
    IF parent_org <> NEW.org_id THEN
        RAISE EXCEPTION
            'agent_prompt_versions.org_id % does not match parent agent % org %',
            NEW.org_id, NEW.agent_id, parent_org;
    END IF;
    RETURN NEW;
END
$$;

CREATE TRIGGER agent_prompt_versions_enforce_org
    BEFORE INSERT OR UPDATE OF org_id, agent_id ON agent_prompt_versions
    FOR EACH ROW
    EXECUTE FUNCTION enforce_agent_prompt_versions_org();

ALTER TABLE agent_prompt_versions ENABLE ROW LEVEL SECURITY;
ALTER TABLE agent_prompt_versions FORCE ROW LEVEL SECURITY;
CREATE POLICY agent_prompt_versions_org_isolation ON agent_prompt_versions
    FOR ALL TO PUBLIC
    USING      (app_user_is_member(org_id))
    WITH CHECK (app_user_is_member(org_id));

-- Seed version = 1 for every existing agent so turn_metrics inserts (migration
-- 44) always have a foreign-key target. `system_prompt` is copied verbatim
-- from the live row; `edited_by` is NULL (system seed); `created_at` is the
-- agent's own created_at so the first version's anchor is the agent's birth,
-- not the migration run.
--
-- `ON CONFLICT DO NOTHING` against the `(agent_id, version)` UNIQUE so the
-- seed is idempotent: an operator who hand-ran this insert before the
-- migration landed (or a half-applied recovery from backup) doesn't trip
-- a constraint failure on the second pass.
INSERT INTO agent_prompt_versions (id, agent_id, org_id, version, system_prompt, edited_by, created_at)
SELECT gen_random_uuid(), a.id, a.org_id, 1, a.system_prompt, NULL, a.created_at
FROM agents a
ON CONFLICT (agent_id, version) DO NOTHING;
