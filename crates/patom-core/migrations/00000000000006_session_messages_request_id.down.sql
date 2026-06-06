DROP INDEX IF EXISTS session_messages_request_id_idx;
ALTER TABLE session_messages DROP COLUMN IF EXISTS request_id;
