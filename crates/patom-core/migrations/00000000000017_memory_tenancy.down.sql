-- Reverse migration 17. Drop policies → triggers → indexes → columns,
-- then the shared parity-trigger function once the last trigger is gone.

DROP POLICY IF EXISTS validation_events_org_isolation ON validation_events;
ALTER TABLE validation_events DISABLE ROW LEVEL SECURITY;
DROP TRIGGER IF EXISTS validation_events_enforce_org ON validation_events;
DROP INDEX IF EXISTS validation_events_org_idx;
ALTER TABLE validation_events DROP COLUMN IF EXISTS org_id;

DROP POLICY IF EXISTS reflection_checkpoints_org_isolation ON reflection_checkpoints;
ALTER TABLE reflection_checkpoints DISABLE ROW LEVEL SECURITY;
DROP TRIGGER IF EXISTS reflection_checkpoints_enforce_org ON reflection_checkpoints;
DROP INDEX IF EXISTS reflection_checkpoints_org_idx;
ALTER TABLE reflection_checkpoints DROP COLUMN IF EXISTS org_id;

DROP POLICY IF EXISTS contradiction_events_org_isolation ON contradiction_events;
ALTER TABLE contradiction_events DISABLE ROW LEVEL SECURITY;
DROP TRIGGER IF EXISTS contradiction_events_enforce_org ON contradiction_events;
DROP INDEX IF EXISTS contradiction_events_org_idx;
ALTER TABLE contradiction_events DROP COLUMN IF EXISTS org_id;

DROP POLICY IF EXISTS agent_memories_org_isolation ON agent_memories;
ALTER TABLE agent_memories DISABLE ROW LEVEL SECURITY;
DROP TRIGGER IF EXISTS agent_memories_enforce_org ON agent_memories;
DROP INDEX IF EXISTS agent_memories_org_idx;
ALTER TABLE agent_memories DROP COLUMN IF EXISTS org_id;

DROP POLICY IF EXISTS memory_events_org_isolation ON memory_events;
ALTER TABLE memory_events DISABLE ROW LEVEL SECURITY;
DROP TRIGGER IF EXISTS memory_events_enforce_org ON memory_events;
DROP INDEX IF EXISTS memory_events_org_idx;
ALTER TABLE memory_events DROP COLUMN IF EXISTS org_id;

DROP FUNCTION IF EXISTS enforce_memory_row_parent_org();
