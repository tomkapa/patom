-- Reverse of 66. Lossy by design: no default agent is re-elected (the column
-- returns all-FALSE) and DM counterparts / posted-row idempotency keys are
-- dropped.

DROP INDEX thread_messages_idem_unique;
ALTER TABLE thread_messages DROP COLUMN idempotency_key;

DROP INDEX threads_dm_pair_idx;
ALTER TABLE threads DROP CONSTRAINT threads_channel_xor_counterpart;
ALTER TABLE threads DROP COLUMN dm_counterpart_colleague_id;

ALTER TABLE agents ADD COLUMN is_default BOOLEAN NOT NULL DEFAULT FALSE;
CREATE UNIQUE INDEX agents_default_unique
    ON agents (org_id)
    WHERE is_default;
