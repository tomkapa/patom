-- Reverse of 00000000000009_lifecycle_and_librarian.up.sql.

DROP INDEX IF EXISTS agent_memories_embedding_hnsw;
ALTER TABLE agent_memories
    ALTER COLUMN embedding TYPE vector USING embedding::vector;

DROP TABLE IF EXISTS validation_events;

ALTER TABLE memory_events
    DROP CONSTRAINT IF EXISTS memory_events_payload_shape;

ALTER TABLE memory_events
    DROP COLUMN IF EXISTS kind,
    DROP COLUMN IF EXISTS state,
    DROP COLUMN IF EXISTS pinned;
