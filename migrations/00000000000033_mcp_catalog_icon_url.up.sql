-- MCP catalog tile icon.
--
-- `mcp_catalog.icon_url` carries the public URL of a per-integration
-- logo so the frontend can render a branded tile instead of the generic
-- Monogram fallback. Org-scoped rows mutate this column via the upload
-- API (`POST /api/uploads/mcp-catalog/:id`, owner/admin only); built-in
-- (org_id IS NULL) rows are seeded once below and treated as immutable
-- by the upload route.
--
-- The 2048-byte cap mirrors `homepage_url` so list responses can't bloat
-- on a runaway URL, and matches the `ASSET_URL_MAX_LEN` limit in
-- `src/assets/limits.rs` (boundary parser rejects past that).
--
-- Built-in URLs point at `assets.relay.dev` (R2 custom domain). The
-- four object keys (`mcp/notion.svg` etc.) must be uploaded to the R2
-- bucket out-of-band BEFORE this migration runs in any environment;
-- without them the FE renders 404s for the four built-in tiles until
-- the SVGs land. The catalog row is still queryable — only the `<img>`
-- request 404s — so the FE's fallback-to-Monogram path keeps the page
-- usable.
--
-- CLAUDE.md §14: paired down migration drops the column.

ALTER TABLE mcp_catalog
    ADD COLUMN icon_url TEXT
    CHECK (icon_url IS NULL OR octet_length(icon_url) <= 2048);

UPDATE mcp_catalog
   SET icon_url = 'https://assets.relay.dev/mcp/notion.svg'
 WHERE id = 'notion' AND org_id IS NULL;

UPDATE mcp_catalog
   SET icon_url = 'https://assets.relay.dev/mcp/linear.svg'
 WHERE id = 'linear' AND org_id IS NULL;

UPDATE mcp_catalog
   SET icon_url = 'https://assets.relay.dev/mcp/slack.svg'
 WHERE id = 'slack' AND org_id IS NULL;

UPDATE mcp_catalog
   SET icon_url = 'https://assets.relay.dev/mcp/jira.svg'
 WHERE id = 'jira' AND org_id IS NULL;
