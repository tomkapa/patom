-- Revert Google-managed MCP catalog entries.
--
-- Deletes only the built-in (org_id IS NULL) rows seeded by the paired
-- up migration. Any org-scoped rows that happen to shadow these ids
-- (org_id IS NOT NULL) are left intact — they belong to tenants and
-- this migration must not touch them.

DELETE FROM mcp_catalog
 WHERE org_id IS NULL
   AND id IN ('gmail', 'gcal');
