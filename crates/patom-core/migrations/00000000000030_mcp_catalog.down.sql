DROP POLICY IF EXISTS mcp_catalog_delete ON mcp_catalog;
DROP POLICY IF EXISTS mcp_catalog_update ON mcp_catalog;
DROP POLICY IF EXISTS mcp_catalog_insert ON mcp_catalog;
DROP POLICY IF EXISTS mcp_catalog_select ON mcp_catalog;

DROP INDEX IF EXISTS mcp_catalog_id_lookup_idx;
DROP INDEX IF EXISTS mcp_catalog_org_id_key;
DROP INDEX IF EXISTS mcp_catalog_global_id_key;

DROP TABLE IF EXISTS mcp_catalog;
