-- Restore the 32-byte floor on `mcp_oauth_pending.state`. Any pending
-- rows with a shorter `state` (rmcp-generated CSRF tokens) will block
-- the rollback; drain them first via a TTL pass or `TRUNCATE
-- mcp_oauth_pending` before reverting.

ALTER TABLE mcp_oauth_pending
    DROP CONSTRAINT mcp_oauth_pending_state_check,
    ADD CONSTRAINT mcp_oauth_pending_state_check
        CHECK (octet_length(state) BETWEEN 32 AND 128);
