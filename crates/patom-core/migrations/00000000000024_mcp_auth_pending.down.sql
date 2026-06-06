-- Reverse the CHECK widening. Any rows currently in 'auth_pending' have
-- to land somewhere the old constraint accepts; the operator equivalent
-- is "this server needs re-authorisation", so map them to
-- 'reconnect_required' before tightening.
UPDATE mcp_servers
    SET connection_status = 'reconnect_required'
    WHERE connection_status = 'auth_pending';

ALTER TABLE mcp_servers
    DROP CONSTRAINT mcp_servers_connection_status_check;

ALTER TABLE mcp_servers
    ADD CONSTRAINT mcp_servers_connection_status_check
        CHECK (connection_status IN ('ok', 'reconnect_required', 'error'));
