-- Reverse 00000000000010_system_participant.up.sql.
--
-- Safe to run only on a database where no row uses the `'system'` participant
-- kind. The `IF EXISTS` guards make this idempotent against partial up runs;
-- the recreated CHECKs use the original ('human','agent') domain.

ALTER TABLE reflection_checkpoints
    DROP COLUMN IF EXISTS reflection_session_ids;

ALTER TABLE session_messages
    DROP CONSTRAINT IF EXISTS session_messages_receiver_kind_check,
    ADD  CONSTRAINT session_messages_receiver_kind_check
         CHECK (receiver_kind IN ('human','agent'));

ALTER TABLE sessions
    DROP CONSTRAINT IF EXISTS sessions_participant_a_kind_check,
    DROP CONSTRAINT IF EXISTS sessions_participant_b_kind_check,
    ADD  CONSTRAINT sessions_participant_a_kind_check
         CHECK (participant_a_kind IN ('human','agent')),
    ADD  CONSTRAINT sessions_participant_b_kind_check
         CHECK (participant_b_kind IN ('human','agent'));
