-- Reverse migration 14.

DROP POLICY IF EXISTS mcp_servers_org_isolation ON mcp_servers;
DROP INDEX IF EXISTS mcp_servers_org_enabled_idx;
CREATE INDEX mcp_servers_enabled_idx
    ON mcp_servers (alias) WHERE enabled = TRUE;
ALTER TABLE mcp_servers DROP CONSTRAINT IF EXISTS mcp_servers_org_alias_key;
ALTER TABLE mcp_servers ADD CONSTRAINT mcp_servers_alias_key UNIQUE (alias);
ALTER TABLE mcp_servers DROP COLUMN IF EXISTS org_id;
ALTER TABLE mcp_servers DISABLE ROW LEVEL SECURITY;

DROP FUNCTION IF EXISTS app_user_is_member(UUID);
DROP FUNCTION IF EXISTS app_current_user_id();

DROP TABLE IF EXISTS oauth_login_states;
DROP TABLE IF EXISTS org_members;
DROP TABLE IF EXISTS organizations;
DROP TABLE IF EXISTS user_identities;
DROP TABLE IF EXISTS users;

-- Drop the RLS role last. `DROP OWNED` removes any per-schema default
-- privileges that still reference `patom_app`; `DROP ROLE` then
-- removes the role itself. `patom_app` is database-wide (NOT
-- per-schema), so this only runs once even though every schema-pinned
-- test re-runs the migration set.
--
-- Skip if dependent objects still reference the role — the test
-- harness runs multiple schemas in parallel and only the last one
-- through is safe to drop the role globally. `DROP OWNED` is
-- idempotent across the per-schema attempts.
DO $$
DECLARE s text := current_schema();
BEGIN
    IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'patom_app') THEN
        EXECUTE format('ALTER DEFAULT PRIVILEGES IN SCHEMA %I REVOKE ALL ON TABLES FROM patom_app', s);
        EXECUTE format('ALTER DEFAULT PRIVILEGES IN SCHEMA %I REVOKE ALL ON FUNCTIONS FROM patom_app', s);
        EXECUTE format('REVOKE ALL ON ALL TABLES IN SCHEMA %I FROM patom_app', s);
        EXECUTE format('REVOKE ALL ON ALL FUNCTIONS IN SCHEMA %I FROM patom_app', s);
        EXECUTE format('REVOKE USAGE ON SCHEMA %I FROM patom_app', s);
        -- DROP OWNED clears default-privileges and any grants in *this*
        -- schema that still mention patom_app, satisfying DROP ROLE's
        -- "no dependent objects" precondition.
        BEGIN
            EXECUTE 'DROP OWNED BY patom_app';
            EXECUTE 'DROP ROLE patom_app';
        EXCEPTION
            WHEN dependent_objects_still_exist THEN
                -- Other test schemas still reference the role; the last
                -- one through wins. Leaving the role in place is safe;
                -- it has no LOGIN and no remaining privileges.
                NULL;
            WHEN insufficient_privilege THEN
                NULL;
        END;
    END IF;
END
$$;
