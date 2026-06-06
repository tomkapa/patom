-- mcp_servers.alias → catalog_id (R2 follow-up).
--
-- The alias was operator-chosen free text; it now becomes a reference to
-- `mcp_catalog(id)`. The tool-prefix that the registry builds for each
-- exposed MCP tool (`mcp_<catalog_id>_<remote_name>`) keys off this
-- column instead.
--
-- Postgres can't express the FK directly because the parent uses a
-- partial unique index keyed on `(id)` (global rows) OR `(org_id, id)`
-- (org-scoped rows); we encode the "row exists either globally or in
-- your org" rule with a trigger.
--
-- Pre-launch + `feedback_no_backcompat`: dev DB is wiped, the column is
-- replaced wholesale, no backfill columns.

-- ───────────────────────────────────────────────────────────────────────────
-- Replace the column.
-- ───────────────────────────────────────────────────────────────────────────

ALTER TABLE mcp_servers DROP CONSTRAINT IF EXISTS mcp_servers_org_alias_key;
DROP INDEX IF EXISTS mcp_servers_org_enabled_idx;
ALTER TABLE mcp_servers DROP COLUMN alias;

ALTER TABLE mcp_servers
    ADD COLUMN catalog_id TEXT NOT NULL
        CHECK (catalog_id ~ '^[a-z][a-z0-9_-]{0,39}$');

-- One wired connection per (org, catalog). A tenant wanting two Notion
-- workspaces would need a custom catalog id (different `id` value); the
-- one-per-catalog rule keeps the recruiter's catalog_id → server_id
-- resolution unambiguous.
ALTER TABLE mcp_servers
    ADD CONSTRAINT mcp_servers_org_catalog_key UNIQUE (org_id, catalog_id);

-- Refresh walks enabled rows in org-key order; this is the new shape of
-- the partial index migration 14 originally built on (org_id, alias).
CREATE INDEX mcp_servers_org_enabled_idx
    ON mcp_servers (org_id, catalog_id) WHERE enabled = TRUE;

-- ───────────────────────────────────────────────────────────────────────────
-- Validation trigger — replaces the missing FK.
--
-- A row in `mcp_servers` is valid iff a catalog entry exists whose `id`
-- matches `catalog_id` AND whose `org_id` is either NULL (global) or
-- equal to the row's `org_id` (tenant-custom). Trigger fires on INSERT
-- and UPDATE OF (catalog_id, org_id). DEFERRABLE would not buy us
-- anything because the catalog row must already exist before the
-- mcp_servers row is created.
-- ───────────────────────────────────────────────────────────────────────────

CREATE OR REPLACE FUNCTION mcp_servers_validate_catalog() RETURNS trigger
    LANGUAGE plpgsql STABLE SECURITY DEFINER AS $$
BEGIN
    -- SECURITY DEFINER so the catalog existence check is RLS-bypassing
    -- and runs as the table owner — without it, a tenant-scoped tx
    -- can't see a global catalog row inside the trigger if the row was
    -- inserted by a different role's connection. The visibility check
    -- proper still happens at the policy level on the catalog itself.
    IF NOT EXISTS (
        SELECT 1 FROM mcp_catalog
         WHERE id = NEW.catalog_id
           AND (org_id IS NULL OR org_id = NEW.org_id)
    ) THEN
        RAISE EXCEPTION
            'mcp_servers.catalog_id ''%'' does not resolve to a global or org-scoped catalog entry for org %',
            NEW.catalog_id, NEW.org_id
            USING ERRCODE = 'foreign_key_violation';
    END IF;
    RETURN NEW;
END;
$$;

-- Lock the function's search_path so a hijacked search_path can't
-- redirect `mcp_catalog` to a different table (same pattern as
-- `app_user_is_member` in migration 14).
DO $$
DECLARE s text := current_schema();
BEGIN
    EXECUTE format(
        'ALTER FUNCTION mcp_servers_validate_catalog() SET search_path = %I, pg_catalog',
        s
    );
END
$$;

CREATE TRIGGER mcp_servers_validate_catalog_trg
    BEFORE INSERT OR UPDATE OF catalog_id, org_id ON mcp_servers
    FOR EACH ROW
    EXECUTE FUNCTION mcp_servers_validate_catalog();
