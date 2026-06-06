-- Roll back to the server-only allowlist. Pre-launch: existing per-tool
-- subsets are not preserved — every row resets to an empty allowlist.
ALTER TABLE agents DROP CONSTRAINT agents_allowed_mcp_tools_shape;
ALTER TABLE agents DROP COLUMN allowed_mcp_tools;
DROP FUNCTION IF EXISTS agents_allowed_mcp_tools_valid(jsonb);

ALTER TABLE agents
    ADD COLUMN allowed_mcp_servers UUID[] NOT NULL DEFAULT '{}'
        CHECK (cardinality(allowed_mcp_servers) <= 32);
