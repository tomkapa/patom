-- Scheduling tenancy retrofit. Final per-table retrofit in the series
-- (foundation 14, agents 15, sessions 16, memory 17, runtime 18). Adds
-- `org_id` + `created_by_user_id` to `scheduled_tasks` so the fire-tx
-- enqueues a `prompt_requests` row directly from the task's stored
-- tenancy — the `tenancy_for_agent` JOIN through `org_members` introduced
-- in the sessions retrofit (sched scheduler.rs) goes away in the same PR.
--
-- The parity trigger pins `scheduled_tasks.org_id` to the owning agent's
-- `agents.org_id` (a CHECK can't reference another row). The shared
-- helper idiom from migrations 17/18 is reused — this is the only
-- scheduling-side table with a parent-org parity check.
--
-- The scheduler scans cross-tenant via `begin_privileged`, so the row's
-- `org_id` is read out of the SELECT and flows into the enqueued
-- `NewPromptRequest`. The store's per-owner paths (cancel, list) stay
-- on `begin_privileged` to mirror the prior retrofits; tenant safety on
-- those paths is enforced by the tool layer opening `begin_as_user`
-- before calling the store.
--
-- Pre-launch: NOT NULL with no backfill (see `feedback_no_backcompat`).
-- Existing dev rows must be wiped before applying; the test schema is
-- minted fresh per test so the test path is unaffected.

CREATE OR REPLACE FUNCTION enforce_scheduled_tasks_parent_org() RETURNS TRIGGER
    LANGUAGE plpgsql AS $$
DECLARE
    parent_org UUID;
BEGIN
    SELECT org_id INTO parent_org FROM agents WHERE id = NEW.owner_agent_id;
    IF parent_org IS NULL THEN
        RAISE EXCEPTION
            'scheduled_tasks.owner_agent_id % references missing agent',
            NEW.owner_agent_id;
    END IF;
    IF parent_org <> NEW.org_id THEN
        RAISE EXCEPTION
            'scheduled_tasks.org_id % does not match parent agent % org %',
            NEW.org_id, NEW.owner_agent_id, parent_org;
    END IF;
    RETURN NEW;
END
$$;

ALTER TABLE scheduled_tasks
    ADD COLUMN org_id UUID NOT NULL REFERENCES organizations(id) ON DELETE CASCADE;

ALTER TABLE scheduled_tasks
    ADD COLUMN created_by_user_id UUID NOT NULL REFERENCES users(id);

-- Per-owner partial index re-cut with `org_id` leading so future
-- per-org list paths can use it without a separate index. The
-- per-owner clause stays unchanged (it's what `list_for_agent` filters
-- on); leading-org is forwards-compatible.
DROP INDEX IF EXISTS scheduled_tasks_owner_active_idx;
CREATE INDEX scheduled_tasks_owner_active_idx
    ON scheduled_tasks (org_id, owner_agent_id)
    WHERE state = 'active';

-- The existing `scheduled_tasks_due_idx` (state='active' AND next_run_at
-- IS NOT NULL) stays as-is — the scheduler scans across all orgs via
-- `begin_privileged`, so an org-leading index would actively hurt that
-- path.

CREATE TRIGGER scheduled_tasks_enforce_org
    BEFORE INSERT OR UPDATE OF org_id, owner_agent_id ON scheduled_tasks
    FOR EACH ROW
    EXECUTE FUNCTION enforce_scheduled_tasks_parent_org();

ALTER TABLE scheduled_tasks ENABLE ROW LEVEL SECURITY;
ALTER TABLE scheduled_tasks FORCE ROW LEVEL SECURITY;
CREATE POLICY scheduled_tasks_org_isolation ON scheduled_tasks
    FOR ALL TO PUBLIC
    USING      (app_user_is_member(org_id))
    WITH CHECK (app_user_is_member(org_id));
