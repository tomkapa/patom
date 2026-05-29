-- MCP OAuth subsystem refactor.
--
-- Three shape changes in one file:
--
--   1. `mcp_catalog` gains `client_source` ∈ {platform, dcr, none} and an
--      optional `platform_client_alias` so two catalog entries can share
--      one platform OAuth client (Gmail + Calendar both point at the
--      Google app via `alias = 'google'`). Pure data, no vendor
--      branches in code.
--
--   2. `mcp_oauth_pending` gains DCR-client columns so the freshly
--      registered (`client_id`, encrypted `client_secret`, auth method)
--      can ride from `POST /oauth/start` to `GET /oauth/callback`
--      without a separate DCR-clients table. The `issuer` column is
--      redundant (server links to catalog) — drop it in the same hop.
--
--   3. `mcp_oauth_clients` is dropped entirely. Platform vendors read
--      from env (`PATOM_<X>_CLIENT_ID/_SECRET`); DCR vendors persist
--      their issued client material inside the encrypted `OAuth2Payload`
--      in `mcp_server_credentials`. One row per server holds the access
--      token + refresh token + DCR client material — closing the
--      `(table, issuer, org)` lookup the old store needed.
--
-- Pre-launch: no backfill of the dropped table.

ALTER TABLE mcp_catalog
    ADD COLUMN client_source TEXT NOT NULL DEFAULT 'dcr'
        CHECK (client_source IN ('platform','dcr','none')),
    ADD COLUMN platform_client_alias TEXT
        CHECK (platform_client_alias IS NULL
            OR platform_client_alias ~ '^[a-z][a-z0-9_-]{0,39}$');

-- Backfill: platform-supported vendors that ship today.
UPDATE mcp_catalog SET client_source = 'platform'
 WHERE id IN ('google', 'github') AND org_id IS NULL;

-- Gmail + Calendar share Google's OAuth app via alias = 'google'.
UPDATE mcp_catalog SET client_source = 'platform', platform_client_alias = 'google'
 WHERE id IN ('gmail', 'gcal') AND org_id IS NULL;

-- `auth_kind = 'none'` rows (anonymous MCP servers) carry no OAuth client
-- at all; mark client_source = 'none' so the resolver short-circuits.
UPDATE mcp_catalog SET client_source = 'none'
 WHERE auth_kind = 'none';

-- mcp_oauth_pending: drop the redundant issuer column, add DCR-client
-- handoff columns so the freshly-registered client_id/secret/auth_method
-- crosses start → callback.
ALTER TABLE mcp_oauth_pending
    DROP COLUMN issuer,
    ADD COLUMN dcr_client_id TEXT
        CHECK (dcr_client_id IS NULL OR octet_length(dcr_client_id) BETWEEN 1 AND 1024),
    ADD COLUMN dcr_client_secret_ciphertext BYTEA,
    ADD COLUMN dcr_client_secret_nonce BYTEA
        CHECK (
            dcr_client_secret_nonce IS NULL OR octet_length(dcr_client_secret_nonce) = 12
        ),
    ADD COLUMN dcr_token_endpoint_auth_method TEXT
        CHECK (
            dcr_token_endpoint_auth_method IS NULL
            OR dcr_token_endpoint_auth_method IN ('none','client_secret_basic','client_secret_post')
        ),
    ADD COLUMN dcr_authorization_endpoint TEXT
        CHECK (
            dcr_authorization_endpoint IS NULL
            OR octet_length(dcr_authorization_endpoint) BETWEEN 1 AND 2048
        ),
    ADD COLUMN dcr_token_endpoint TEXT
        CHECK (
            dcr_token_endpoint IS NULL
            OR octet_length(dcr_token_endpoint) BETWEEN 1 AND 2048
        ),
    ADD CONSTRAINT mcp_oauth_pending_dcr_secret_pair CHECK (
        (dcr_client_secret_ciphertext IS NULL) = (dcr_client_secret_nonce IS NULL)
    ),
    -- 4-way all-or-none on the non-secret DCR fields. The Rust decoder
    -- mirrors this with an exhaustive match; without the CHECK a partial
    -- INSERT (manual DB edit, future code path) would trip the panic
    -- branch and abort the process (CLAUDE.md §6 + panic = abort).
    ADD CONSTRAINT mcp_oauth_pending_dcr_all_or_none CHECK (
        (dcr_client_id IS NULL) = (dcr_token_endpoint_auth_method IS NULL)
        AND (dcr_client_id IS NULL) = (dcr_authorization_endpoint IS NULL)
        AND (dcr_client_id IS NULL) = (dcr_token_endpoint IS NULL)
    );

DROP TABLE IF EXISTS mcp_oauth_clients;
