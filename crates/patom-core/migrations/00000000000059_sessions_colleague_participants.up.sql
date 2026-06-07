-- Sessions + session_messages: addressing flips from `(kind, agent_id)` pairs
-- to a single `colleague_id` FK per end. Reset-allowed (no backfill — small
-- friend-only user base; see [[product-in-production-migrations-need-backfill]]
-- memory): the DB is wiped before this migration runs, so the new shape lands
-- fresh. The .down recreates the prior shape but no data-migration symmetry.
--
-- Canonical ordering moves from kind-lex to pure UUID on `colleague_id`. The
-- synthetic `System` end is the NULL convention — `participant_b_colleague_id`
-- is the only nullable side, and `Participant::canonical_cmp` sorts System
-- last so the real colleague always lands in slot `a`. `NULLS NOT DISTINCT`
-- keeps the dedup tight when slot `b` is NULL (two reflection sessions for
-- the same agent would otherwise both insert).

-- ───────────────────────────────────────────────────────────────────────────
-- sessions
-- ───────────────────────────────────────────────────────────────────────────
ALTER TABLE sessions
    DROP CONSTRAINT IF EXISTS sessions_participants_distinct,
    DROP CONSTRAINT IF EXISTS sessions_a_kind_agent,
    DROP CONSTRAINT IF EXISTS sessions_b_kind_agent;
DROP INDEX IF EXISTS sessions_dag_pair_unique;

ALTER TABLE sessions
    DROP COLUMN participant_a_kind,
    DROP COLUMN participant_a_agent_id,
    DROP COLUMN participant_b_kind,
    DROP COLUMN participant_b_agent_id;

ALTER TABLE sessions
    ADD COLUMN participant_a_colleague_id UUID NOT NULL REFERENCES colleagues(id),
    -- `participant_b_colleague_id IS NULL` encodes `Participant::System`. Slot
    -- `b` is the only nullable end; canonical ordering puts every real
    -- colleague in slot `a`.
    ADD COLUMN participant_b_colleague_id UUID NULL REFERENCES colleagues(id),
    ADD CONSTRAINT sessions_participants_distinct CHECK (
        participant_b_colleague_id IS NULL
     OR participant_a_colleague_id < participant_b_colleague_id
    );

-- One session per `(org, DAG, canonical colleague pair)`. NULLS NOT DISTINCT
-- treats two `(a, NULL)` rows as equal so the System slot dedupes correctly.
CREATE UNIQUE INDEX sessions_dag_pair_unique
    ON sessions (org_id, root_request_id,
                 participant_a_colleague_id, participant_b_colleague_id)
    NULLS NOT DISTINCT;

-- ───────────────────────────────────────────────────────────────────────────
-- session_messages
-- ───────────────────────────────────────────────────────────────────────────
ALTER TABLE session_messages
    DROP CONSTRAINT IF EXISTS session_messages_sender_kind_agent,
    DROP CONSTRAINT IF EXISTS session_messages_receiver_kind_agent;

ALTER TABLE session_messages
    DROP COLUMN sender_kind,
    DROP COLUMN sender_agent_id,
    DROP COLUMN receiver_kind,
    DROP COLUMN receiver_agent_id;

ALTER TABLE session_messages
    -- `sender_colleague_id IS NULL` encodes the worker-injected `System`
    -- sender (ping-pong nudge). Receivers are never System.
    ADD COLUMN sender_colleague_id   UUID NULL REFERENCES colleagues(id),
    ADD COLUMN receiver_colleague_id UUID NOT NULL REFERENCES colleagues(id);
