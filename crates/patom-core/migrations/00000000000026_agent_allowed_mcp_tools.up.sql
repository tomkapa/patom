-- Per-agent MCP **tool** allowlist (supersedes the server-only allowlist).
--
-- The prior column, `agents.allowed_mcp_servers UUID[]`, granted an agent
-- access to every tool exposed by a server. The new column carries
-- per-server tool subsets so an operator can say "this agent may use
-- Linear, but only `issues.create` and `issues.search`."
--
-- Shape:
--   {
--     "<server_uuid>": null,                       -- all tools from this server
--     "<server_uuid>": ["issues.create", ...]      -- only these remote tool names
--   }
-- A server uuid that is *absent* from the top-level object means the
-- agent has no access to that server's tools. This matches the strict
-- semantics of the old column: nothing is implicit, every grant is
-- explicit.
--
-- Two caps live in the validator:
--   * top-level object has ≤ 32 keys (mirrors `MAX_MCP_SERVERS`)
--   * each value is `NULL` or a JSON array of ≤ 64 strings, each
--     1..=64 bytes, no duplicates within the array (mirrors
--     `MAX_TOOLS_PER_SERVER` and `TOOL_NAME_MAX_LEN`).
-- Postgres forbids subqueries inside CHECK constraints (SQL standard), so
-- the validation lives in an `IMMUTABLE` PL/pgSQL function that the CHECK
-- delegates to.
--
-- CLAUDE.md §13: pre-launch, no nullable shim — the old column goes in the
-- same migration.

ALTER TABLE agents DROP COLUMN allowed_mcp_servers;

CREATE OR REPLACE FUNCTION agents_allowed_mcp_tools_valid(payload jsonb)
RETURNS boolean
LANGUAGE plpgsql
IMMUTABLE
AS $$
DECLARE
    key_count int;
    kv record;
    elem text;
    seen_count int;
    distinct_count int;
BEGIN
    IF jsonb_typeof(payload) <> 'object' THEN
        RETURN false;
    END IF;
    SELECT count(*) INTO key_count
    FROM jsonb_object_keys(payload);
    IF key_count > 32 THEN
        RETURN false;
    END IF;
    FOR kv IN SELECT key, value FROM jsonb_each(payload) LOOP
        IF jsonb_typeof(kv.value) = 'null' THEN
            CONTINUE;
        END IF;
        IF jsonb_typeof(kv.value) <> 'array' THEN
            RETURN false;
        END IF;
        IF jsonb_array_length(kv.value) > 64 THEN
            RETURN false;
        END IF;
        FOR elem IN
            SELECT jsonb_array_elements(kv.value) #>> '{}'
        LOOP
            IF elem IS NULL THEN
                RETURN false;
            END IF;
            IF octet_length(elem) < 1 OR octet_length(elem) > 64 THEN
                RETURN false;
            END IF;
        END LOOP;
        SELECT count(*), count(DISTINCT v)
          INTO seen_count, distinct_count
          FROM jsonb_array_elements_text(kv.value) AS v;
        IF seen_count <> distinct_count THEN
            RETURN false;
        END IF;
    END LOOP;
    RETURN true;
END;
$$;

ALTER TABLE agents
    ADD COLUMN allowed_mcp_tools JSONB NOT NULL DEFAULT '{}'::jsonb;

ALTER TABLE agents
    ADD CONSTRAINT agents_allowed_mcp_tools_shape
    CHECK (agents_allowed_mcp_tools_valid(allowed_mcp_tools));
