DROP INDEX IF EXISTS mcp_servers_created_by_idx;
ALTER TABLE mcp_servers DROP COLUMN IF EXISTS created_by_user_id;
