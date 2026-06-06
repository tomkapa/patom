DROP TABLE IF EXISTS reflection_checkpoints;
DROP TABLE IF EXISTS contradiction_events;
DROP TABLE IF EXISTS agent_memories;
DROP TABLE IF EXISTS memory_events;

ALTER TABLE prompt_requests DROP COLUMN IF EXISTS kind_payload;
ALTER TABLE prompt_requests DROP COLUMN IF EXISTS kind;
