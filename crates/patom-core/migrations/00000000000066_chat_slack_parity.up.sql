-- Chat Slack-parity (doc/chat-ui-slack-parity-plan.md):
--   1. The default-agent concept is deleted. The recruiter stays as the preset
--      agent seeded at org creation; nothing is "default" at runtime any more.
--   2. DMs gain a counterpart: a channel-less thread is a conversation between
--      its creator and `dm_counterpart_colleague_id` (human OR agent), visible
--      to both. The counterpart constrains *human visibility only* — any agent
--      can still be invoked into the thread (agents are org-global).
--   3. Human posted rows carry the submit's idempotency key so an untagged
--      post (which enqueues no trigger row) still dedupes on retry, and the FE
--      reconciles its optimistic bubble by the key it minted.

-- ── 1. default agent ────────────────────────────────────────────────────────
-- Dropping the column also drops the `agents_default_unique` partial index.
ALTER TABLE agents DROP COLUMN is_default;

-- ── 2. DM counterpart ───────────────────────────────────────────────────────
-- SET NULL (not RESTRICT) so deleting an agent (cascading its colleague row)
-- degrades the DM to a creator-only legacy thread instead of blocking the
-- delete.
ALTER TABLE threads
    ADD COLUMN dm_counterpart_colleague_id UUID NULL
        REFERENCES colleagues(id) ON DELETE SET NULL;

-- Backfill: every pre-existing channel-less thread was a DM with one agent.
-- Counterpart := the earliest-joined participating agent's colleague.
UPDATE threads t
SET dm_counterpart_colleague_id = sub.colleague_id
FROM (
    SELECT DISTINCT ON (ats.thread_id) ats.thread_id, c.id AS colleague_id
    FROM agent_thread_state ats
    JOIN colleagues c ON c.agent_id = ats.agent_id AND c.org_id = ats.org_id
    ORDER BY ats.thread_id, ats.created_at ASC
) sub
WHERE t.id = sub.thread_id AND t.channel_id IS NULL;

-- One-way CHECK: a channel thread never carries a counterpart. The reverse
-- (DM ⇒ counterpart NOT NULL) is enforced at create time in code, NOT here:
-- `threads_channel_org_fk` is ON DELETE SET NULL(channel_id), so a channel
-- removal mid-cascade would otherwise trip a two-way CHECK. A channel-less
-- row with a NULL counterpart is a legacy/degraded DM, visible to its
-- creator only.
ALTER TABLE threads
    ADD CONSTRAINT threads_channel_xor_counterpart
        CHECK (channel_id IS NULL OR dm_counterpart_colleague_id IS NULL);

-- DM pair listing: "threads between me and counterpart C", both orientations.
CREATE INDEX threads_dm_pair_idx
    ON threads (org_id, dm_counterpart_colleague_id, last_activity_at DESC)
    WHERE channel_id IS NULL;

-- ── 3. posted-row idempotency ───────────────────────────────────────────────
-- Same 200-byte cap as `prompt_requests.idempotency_key` (the shared
-- `IdempotencyKey` newtype parses both).
ALTER TABLE thread_messages
    ADD COLUMN idempotency_key TEXT NULL
        CHECK (idempotency_key IS NULL OR char_length(idempotency_key) <= 200);

CREATE UNIQUE INDEX thread_messages_idem_unique
    ON thread_messages (org_id, idempotency_key)
    WHERE idempotency_key IS NOT NULL;
