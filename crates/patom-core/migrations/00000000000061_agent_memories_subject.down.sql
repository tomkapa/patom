-- Reverse of migration 61 (§14: down recreates the prior shape).

-- Restore the prior (narrower) `memory_events.kind` CHECK that excluded
-- `collaborator`. This faithfully recreates the pre-60 shape even though that
-- shape carried the latent gap fixed in the up — §14 reversibility, not bug
-- preservation as intent.
ALTER TABLE memory_events
    DROP CONSTRAINT memory_events_kind_check,
    ADD CONSTRAINT memory_events_kind_check
        CHECK (kind IS NULL OR kind IN ('self', 'other', 'procedure', 'open'));

ALTER TABLE memory_events
    DROP CONSTRAINT IF EXISTS memory_events_subject_only_collaborator,
    DROP COLUMN IF EXISTS subject_colleague_id;

DROP INDEX IF EXISTS agent_memories_subject_idx;

ALTER TABLE agent_memories
    DROP CONSTRAINT IF EXISTS agent_memories_subject_only_collaborator,
    DROP COLUMN IF EXISTS subject_colleague_id;
