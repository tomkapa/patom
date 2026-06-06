-- System participant (doc/memory.md §1.6, §1.8) — reflection and resolution
-- run in their own off-conversation sessions paired against a singleton
-- `Participant::System`. The agent talks to itself for audit; nobody on the
-- "system" side ever speaks back. Schema changes:
--
-- 1. `sessions.participant_*_kind` accepts `'system'` in addition to
--    `'human' | 'agent'`. The canonical-order CHECK keeps working because
--    `'agent' < 'human' < 'system'` lexically.
-- 2. `session_messages.receiver_kind` accepts `'system'`. The
--    `receiver_kind = 'agent' iff receiver_agent_id IS NOT NULL` invariant is
--    unchanged — system rows have NULL agent ids same as human rows.
--    `prompt_requests.receiver_kind` stays restricted to `'agent'`: even for
--    reflection / resolution claims, the receiver agent_id is the agent
--    being driven, not the System side.
-- 3. `reflection_checkpoints.reflection_session_ids` records every reflection
--    session ever minted for `(agent, conversation)`. Append-on-success; the
--    most recent id is at the tail. Replaces the implicit "latest" lookup
--    with an explicit audit list. No FK on array elements (Postgres doesn't
--    support per-element FKs); orphans are tolerable since reflection
--    sessions aren't routinely deleted.

ALTER TABLE sessions
    DROP CONSTRAINT sessions_participant_a_kind_check,
    DROP CONSTRAINT sessions_participant_b_kind_check,
    ADD  CONSTRAINT sessions_participant_a_kind_check
         CHECK (participant_a_kind IN ('human','agent','system')),
    ADD  CONSTRAINT sessions_participant_b_kind_check
         CHECK (participant_b_kind IN ('human','agent','system'));

ALTER TABLE session_messages
    DROP CONSTRAINT session_messages_receiver_kind_check,
    ADD  CONSTRAINT session_messages_receiver_kind_check
         CHECK (receiver_kind IN ('human','agent','system'));

ALTER TABLE reflection_checkpoints
    ADD COLUMN reflection_session_ids UUID[] NOT NULL DEFAULT '{}';
