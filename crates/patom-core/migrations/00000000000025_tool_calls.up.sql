-- tool_calls — one row per tool invocation the agent dispatcher executes.
--
-- Today MCP tool calls survive only as JSONB inside session_messages.body;
-- dashboards that want "calls per MCP connection" or "calls per agent" had
-- to scan every message row and unpack JSON. This table records the access
-- pattern's columns directly with partial indexes on (mcp_server_id, ...)
-- so the two dashboard queries stay cheap as the table grows.
--
-- The table is designed generically: mcp_server_id is NULLABLE, so future
-- non-MCP call types (scheduled tasks, webhooks, …) can reuse the same
-- table by leaving that column null. MCP is the first writer; not the last.
--
-- Pre-launch single-step migration (feedback_no_backcompat): NOT NULL with
-- no backfill. Dev DBs are wiped before applying.

CREATE TABLE tool_calls (
    id              UUID PRIMARY KEY,
    org_id          UUID NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    session_id      UUID NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    request_id      UUID NOT NULL REFERENCES prompt_requests(id) ON DELETE CASCADE,
    agent_id        UUID NOT NULL REFERENCES agents(id),
    -- Nullable so future non-MCP call types reuse this table by leaving
    -- mcp_server_id NULL. ON DELETE SET NULL keeps historical rows
    -- queryable by agent/tool after a connection is removed; only the
    -- "calls per connection" lens loses them, which is intentional.
    mcp_server_id   UUID REFERENCES mcp_servers(id) ON DELETE SET NULL,
    tool_name       TEXT NOT NULL
                    CHECK (octet_length(tool_name) BETWEEN 1 AND 128),
    started_at      TIMESTAMPTZ NOT NULL,
    duration_ms     INTEGER NOT NULL CHECK (duration_ms >= 0),
    is_error        BOOLEAN NOT NULL,
    created_at      TIMESTAMPTZ NOT NULL
);

-- Partial indexes: scoped to MCP rows so future non-MCP rows don't bloat them.
-- Both dashboard queries lead with (mcp_server_id IS NOT NULL), so the planner
-- can pick these indexes without scanning unrelated rows.
CREATE INDEX tool_calls_per_connection_idx
    ON tool_calls (mcp_server_id, started_at DESC)
    WHERE mcp_server_id IS NOT NULL;

CREATE INDEX tool_calls_per_agent_mcp_idx
    ON tool_calls (agent_id, started_at DESC)
    WHERE mcp_server_id IS NOT NULL;

-- Denormalised org_id must match the parent session's. A trigger enforces
-- it because CHECK constraints can't reference other rows. Mirrors
-- enforce_session_messages_org from migration 16.
CREATE OR REPLACE FUNCTION enforce_tool_calls_org() RETURNS TRIGGER
    LANGUAGE plpgsql AS $$
DECLARE
    parent_org UUID;
BEGIN
    SELECT org_id INTO parent_org FROM sessions WHERE id = NEW.session_id;
    IF parent_org IS NULL THEN
        RAISE EXCEPTION
            'tool_calls.session_id % references missing session',
            NEW.session_id;
    END IF;
    IF parent_org <> NEW.org_id THEN
        RAISE EXCEPTION
            'tool_calls.org_id % does not match parent session % org %',
            NEW.org_id, NEW.session_id, parent_org;
    END IF;
    RETURN NEW;
END
$$;

CREATE TRIGGER tool_calls_enforce_org
    BEFORE INSERT OR UPDATE OF org_id, session_id ON tool_calls
    FOR EACH ROW
    EXECUTE FUNCTION enforce_tool_calls_org();

ALTER TABLE tool_calls ENABLE ROW LEVEL SECURITY;
ALTER TABLE tool_calls FORCE ROW LEVEL SECURITY;
CREATE POLICY tool_calls_org_isolation ON tool_calls
    FOR ALL TO PUBLIC
    USING      (app_user_is_member(org_id))
    WITH CHECK (app_user_is_member(org_id));
