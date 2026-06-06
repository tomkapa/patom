ALTER TABLE tool_calls DROP CONSTRAINT IF EXISTS tool_calls_error_message_only_on_error;
ALTER TABLE tool_calls DROP COLUMN IF EXISTS error_message;
