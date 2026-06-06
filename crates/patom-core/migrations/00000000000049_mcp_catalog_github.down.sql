-- Revert GitHub-managed MCP catalog entry.
--
-- Deletes only the built-in (org_id IS NULL) row seeded by the paired up
-- migration. Any org-scoped rows that happen to shadow this id
-- (org_id IS NOT NULL) are left intact — they belong to tenants and this
-- migration must not touch them.

DELETE FROM mcp_catalog
 WHERE org_id IS NULL
   AND id = 'github';
