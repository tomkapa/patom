-- Reverse 00000000000067_mcp_user_supplied_oauth_client.
--
-- Lossy by construction: any row with client_source = 'user_supplied' must be
-- cleared (or repointed) before the 3-value CHECK can be restored. Pre-launch
-- friends-only data set; acceptable per CLAUDE.md §14 rollback discipline.

-- Drop the multi-column / cross-column constraints first so the column drops
-- below don't error (Postgres only auto-drops single-column constraints).
ALTER TABLE mcp_catalog
    DROP CONSTRAINT IF EXISTS mcp_catalog_user_supplied_requires_client_id,
    DROP CONSTRAINT IF EXISTS mcp_catalog_oauth_secret_triple,
    DROP COLUMN IF EXISTS oauth_client_secret_key_version,
    DROP COLUMN IF EXISTS oauth_client_secret_nonce,
    DROP COLUMN IF EXISTS oauth_client_secret_ciphertext,
    DROP COLUMN IF EXISTS oauth_client_id;

-- Restore the 3-value client_source CHECK from migration 50.
ALTER TABLE mcp_catalog
    DROP CONSTRAINT mcp_catalog_client_source_check;

ALTER TABLE mcp_catalog
    ADD CONSTRAINT mcp_catalog_client_source_check
        CHECK (client_source IN ('platform','dcr','none'));
