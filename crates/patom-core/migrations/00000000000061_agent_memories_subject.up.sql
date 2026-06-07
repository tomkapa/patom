-- Memory `Collaborator` keys on a colleague subject (Colleagues plan, Stage 5).
--
-- A `collaborator` memory is the agent's belief about a *specific* coworker
-- ("Designer ships fast but skips tests"). Until now the subject lived only in
-- the prose; this column makes "what I remember about Tom" address the same
-- `colleagues` identity the agent talks to. The owning FK
-- (`agent_memories.agent_id`) is unchanged — humans don't own memories; only the
-- *subject* moves onto the colleague axis.
--
-- Subjects are valid for `collaborator` memories only. The column is nullable so
-- every other kind (and a reflection-written collaborator that never named a
-- subject) leaves it NULL; `ON DELETE SET NULL` keeps a memory alive when the
-- subject coworker leaves the org (the prose still reads; the link just dangles).

ALTER TABLE agent_memories
    ADD COLUMN subject_colleague_id UUID NULL REFERENCES colleagues(id) ON DELETE SET NULL,
    ADD CONSTRAINT agent_memories_subject_only_collaborator CHECK (
        subject_colleague_id IS NULL OR kind = 'collaborator'
    );

-- Partial: only collaborator rows ever carry a subject, and the reverse lookup
-- ("which memories are about colleague X") only cares about non-NULL subjects.
CREATE INDEX agent_memories_subject_idx
    ON agent_memories (subject_colleague_id) WHERE subject_colleague_id IS NOT NULL;

-- Journal replay must reconstruct the subject identically to the live write, so
-- the same nullable column rides on every `write` event. `update` / `forget`
-- leave it NULL (the subject is immutable after the write, and the rebuild
-- salvages it from the write event).
ALTER TABLE memory_events
    ADD COLUMN subject_colleague_id UUID NULL REFERENCES colleagues(id) ON DELETE SET NULL,
    ADD CONSTRAINT memory_events_subject_only_collaborator CHECK (
        subject_colleague_id IS NULL OR kind = 'collaborator'
    );

-- Pre-existing gap: migration 13 added `collaborator` to `agent_memories.kind`
-- but never widened `memory_events.kind` (migration 9). A collaborator write
-- journals a row with `kind = 'collaborator'`, which the stale event CHECK would
-- reject — so no collaborator memory could ever be journaled. Align the event
-- CHECK with the materialized-table CHECK now that subjects make collaborator
-- memories first-class.
ALTER TABLE memory_events
    DROP CONSTRAINT memory_events_kind_check,
    ADD CONSTRAINT memory_events_kind_check
        CHECK (kind IS NULL OR kind IN ('self', 'other', 'collaborator', 'procedure', 'open'));
