-- MCP server registry. Each row is one upstream MCP server we've registered through
-- the HTTP API; refresh() reads this table, opens connections, and exposes the union
-- of remote tools through `McpRegistry` -> `ToolBox` -> the agent.
--
-- `config` is the tagged-union `McpTransport` envelope as JSONB so adding a new
-- transport variant (today: only "http") is a serde change, not a migration.
-- `discovered_tools` caches the last successful tool list so the API can show
-- operators what's currently exposed without round-tripping the upstream.
CREATE TABLE mcp_servers (
    id                UUID PRIMARY KEY,
    alias             TEXT NOT NULL
                      CHECK (octet_length(alias) BETWEEN 1 AND 16),
    enabled           BOOLEAN NOT NULL DEFAULT TRUE,
    config            JSONB NOT NULL,
    description       TEXT CHECK (description IS NULL OR octet_length(description) <= 512),
    last_seen_at      TIMESTAMPTZ,
    last_error        TEXT CHECK (last_error IS NULL OR octet_length(last_error) <= 1024),
    discovered_tools  JSONB,
    created_at        TIMESTAMPTZ NOT NULL,
    updated_at        TIMESTAMPTZ NOT NULL,
    -- task1.md parallel: becomes UNIQUE (user_id, alias) once auth lands.
    UNIQUE (alias)
);

-- Refresh walks enabled rows; partial index keeps the read cheap as the table grows.
CREATE INDEX mcp_servers_enabled_idx
    ON mcp_servers (alias)
    WHERE enabled = TRUE;
