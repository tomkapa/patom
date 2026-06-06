-- Reverse of 00000000000013_agent_discovery.up.sql.
ALTER TABLE agent_memories
    DROP CONSTRAINT IF EXISTS agent_memories_kind_check;
ALTER TABLE agent_memories
    ADD CONSTRAINT agent_memories_kind_check
        CHECK (kind IN ('self','other','procedure','open'));
DROP INDEX IF EXISTS agents_name_lower_unique;
ALTER TABLE agents
    DROP COLUMN IF EXISTS description_embedding,
    DROP COLUMN IF EXISTS description;
