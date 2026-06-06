-- Drops the icon_url column (paired with the `.up.sql`). The CHECK
-- constraint goes with the column, no separate DROP needed.
ALTER TABLE mcp_catalog DROP COLUMN icon_url;
