-- 2048-byte cap mirrors MAX_TOOL_CALL_ERROR_MESSAGE_BYTES in
-- src/tools/limits.rs — the recorder clips before insert, this CHECK is
-- defence in depth.
ALTER TABLE tool_calls
    ADD COLUMN error_message TEXT
    CHECK (error_message IS NULL OR octet_length(error_message) <= 2048);

-- Successful rows must not carry stale error text. Lets the read query
-- assume `is_error=false ⇒ error_message IS NULL` without an extra guard.
ALTER TABLE tool_calls
    ADD CONSTRAINT tool_calls_error_message_only_on_error
    CHECK (is_error OR error_message IS NULL);
