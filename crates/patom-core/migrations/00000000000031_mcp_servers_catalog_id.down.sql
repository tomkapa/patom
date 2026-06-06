DROP TRIGGER IF EXISTS mcp_servers_validate_catalog_trg ON mcp_servers;
DROP FUNCTION IF EXISTS mcp_servers_validate_catalog();

DROP INDEX IF EXISTS mcp_servers_org_enabled_idx;
ALTER TABLE mcp_servers DROP CONSTRAINT IF EXISTS mcp_servers_org_catalog_key;
ALTER TABLE mcp_servers DROP COLUMN catalog_id;

ALTER TABLE mcp_servers
    ADD COLUMN alias TEXT NOT NULL
        CHECK (octet_length(alias) BETWEEN 1 AND 16);

ALTER TABLE mcp_servers
    ADD CONSTRAINT mcp_servers_org_alias_key UNIQUE (org_id, alias);

CREATE INDEX mcp_servers_org_enabled_idx
    ON mcp_servers (org_id, alias) WHERE enabled = TRUE;
