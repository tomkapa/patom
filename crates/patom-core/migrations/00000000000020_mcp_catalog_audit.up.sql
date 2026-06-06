-- MCP catalog audit (R1 — phase A of the catalog/credentials/OAuth bundle).
--
-- Add `created_by_user_id` to `mcp_servers` so list/read responses can show
-- "added by whom" for team visibility. The companion test-connection endpoint
-- (`POST /mcp-servers/test-connect`) needs no schema — it validates without
-- persisting.
--
-- Pre-launch: NOT NULL with no backfill (see `feedback_no_backcompat`).
-- Existing dev rows must be wiped before applying; the test schema is minted
-- fresh per test so the test path is unaffected.

ALTER TABLE mcp_servers
    ADD COLUMN created_by_user_id UUID NOT NULL REFERENCES users(id) ON DELETE RESTRICT;

-- ON DELETE RESTRICT: deleting a user that registered a server fails loudly
-- rather than silently nulling the audit pointer. Operators must reassign or
-- delete the server row first.

CREATE INDEX mcp_servers_created_by_idx
    ON mcp_servers (created_by_user_id);
