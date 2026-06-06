-- Reverse migration 19. Drop policy → trigger → index → columns in
-- dependency-safe order, restore the prior owner-only partial index, then
-- drop the parity-trigger function once its last trigger is gone.

DROP POLICY IF EXISTS scheduled_tasks_org_isolation ON scheduled_tasks;
ALTER TABLE scheduled_tasks DISABLE ROW LEVEL SECURITY;
DROP TRIGGER IF EXISTS scheduled_tasks_enforce_org ON scheduled_tasks;

DROP INDEX IF EXISTS scheduled_tasks_owner_active_idx;
CREATE INDEX scheduled_tasks_owner_active_idx
    ON scheduled_tasks (owner_agent_id)
    WHERE state = 'active';

ALTER TABLE scheduled_tasks DROP COLUMN IF EXISTS created_by_user_id;
ALTER TABLE scheduled_tasks DROP COLUMN IF EXISTS org_id;

DROP FUNCTION IF EXISTS enforce_scheduled_tasks_parent_org();
