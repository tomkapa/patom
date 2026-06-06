-- Widen mcp_servers.connection_status to include 'auth_pending'.
--
-- Background: a row created via POST /mcp-servers for an OAuth-protected
-- endpoint used to default to 'ok' and be picked up by the registry
-- refresh before the user finished the consent flow. The first connect
-- attempt then ran without a Bearer token and the upstream returned
-- "Auth required", surfacing a misleading last_error on a row that was
-- simply waiting for the OAuth callback to persist its token.
--
-- The new state distinguishes "operator hasn't authorised yet" from the
-- existing "ok / reconnect_required / error" lifecycle. `list_enabled`
-- (consumed by the registry refresher) filters it out; the OAuth
-- callback's `mark_connected` flips it to 'ok' once credentials land.

ALTER TABLE mcp_servers
    DROP CONSTRAINT mcp_servers_connection_status_check;

ALTER TABLE mcp_servers
    ADD CONSTRAINT mcp_servers_connection_status_check
        CHECK (connection_status IN ('ok', 'auth_pending', 'reconnect_required', 'error'));
