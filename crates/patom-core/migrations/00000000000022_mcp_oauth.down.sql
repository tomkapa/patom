DROP TABLE IF EXISTS mcp_oauth_pending;
DROP TABLE IF EXISTS mcp_oauth_clients;
ALTER TABLE mcp_servers DROP COLUMN IF EXISTS connection_status;
