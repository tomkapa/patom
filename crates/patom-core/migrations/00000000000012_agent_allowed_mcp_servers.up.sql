-- Per-agent MCP server allowlist.
--
-- Every agent carries an explicit list of MCP server ids it is allowed to
-- expose to the model. There is no "unrestricted" mode: the absence of an id
-- from this array means the agent cannot see any of that server's tools, full
-- stop. A newly-minted agent's array is empty until an operator opts it in.
--
-- Stored as `UUID[]` rather than a join table for atomic-update simplicity:
-- the HTTP PATCH is one `UPDATE agents SET allowed_mcp_servers = $1 WHERE id =
-- $2`, and the read path is one row (no join). Dangling ids (the operator
-- deleted an MCP server whose id is still in some agent's array) are inert
-- because the runtime filter consults the live `McpRegistry` — it only returns
-- tools that exist today.
--
-- Cap mirrors `MAX_MCP_SERVERS = 32` (an agent could legitimately have access
-- to every server registered system-wide); CLAUDE.md §5 — every container has
-- an explicit bound.
ALTER TABLE agents
    ADD COLUMN allowed_mcp_servers UUID[] NOT NULL DEFAULT '{}'
        CHECK (cardinality(allowed_mcp_servers) <= 32);
