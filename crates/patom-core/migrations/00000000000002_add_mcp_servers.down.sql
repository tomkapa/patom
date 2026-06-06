-- Reverse 00000000000002_add_mcp_servers.up.sql.
--
-- The partial index on `enabled = TRUE` is dropped automatically by `DROP TABLE`,
-- but we name it explicitly so the rollback is grep-able and matches the up file.
DROP INDEX IF EXISTS mcp_servers_enabled_idx;
DROP TABLE IF EXISTS mcp_servers;
