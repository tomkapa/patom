-- Reverse migration 50.
--
-- Restores the dropped table + column shapes so a rollout that fails
-- after migration 50 lands but before the new code ships can be unwound.
-- Pre-launch, there are no rows worth preserving.
--
-- Recreate block for `mcp_oauth_clients` is verbatim from migrations
-- 22 + 37.

ALTER TABLE mcp_catalog
    DROP COLUMN platform_client_alias,
    DROP COLUMN client_source;

ALTER TABLE mcp_oauth_pending
    DROP CONSTRAINT mcp_oauth_pending_dcr_all_or_none,
    DROP CONSTRAINT mcp_oauth_pending_dcr_secret_pair,
    DROP COLUMN dcr_token_endpoint,
    DROP COLUMN dcr_authorization_endpoint,
    DROP COLUMN dcr_token_endpoint_auth_method,
    DROP COLUMN dcr_client_secret_nonce,
    DROP COLUMN dcr_client_secret_ciphertext,
    DROP COLUMN dcr_client_id,
    ADD COLUMN issuer TEXT NOT NULL DEFAULT ''
        CHECK (octet_length(issuer) BETWEEN 0 AND 2048);
ALTER TABLE mcp_oauth_pending ALTER COLUMN issuer DROP DEFAULT;

CREATE TABLE mcp_oauth_clients (
    org_id     UUID REFERENCES organizations(id) ON DELETE CASCADE,
    issuer     TEXT NOT NULL CHECK (octet_length(issuer) BETWEEN 1 AND 2048),
    client_id  TEXT NOT NULL CHECK (octet_length(client_id) BETWEEN 1 AND 1024),
    authorization_endpoint TEXT NOT NULL CHECK (octet_length(authorization_endpoint) BETWEEN 1 AND 2048),
    token_endpoint         TEXT NOT NULL CHECK (octet_length(token_endpoint) BETWEEN 1 AND 2048),
    registration_client_uri              TEXT CHECK (registration_client_uri IS NULL OR octet_length(registration_client_uri) <= 2048),
    registration_access_token_ciphertext BYTEA,
    registration_access_token_nonce      BYTEA CHECK (
        registration_access_token_nonce IS NULL OR octet_length(registration_access_token_nonce) = 12
    ),
    client_secret_ciphertext BYTEA,
    client_secret_nonce      BYTEA CHECK (
        client_secret_nonce IS NULL OR octet_length(client_secret_nonce) = 12
    ),
    key_version SMALLINT NOT NULL DEFAULT 1,
    token_endpoint_auth_method TEXT NOT NULL
        CHECK (token_endpoint_auth_method IN ('none','client_secret_basic','client_secret_post')),
    scope       TEXT CHECK (scope IS NULL OR octet_length(scope) <= 2048),
    created_at  TIMESTAMPTZ NOT NULL,
    CHECK (
        (client_secret_ciphertext IS NULL) = (client_secret_nonce IS NULL)
    ),
    CHECK (
        (registration_access_token_ciphertext IS NULL)
        = (registration_access_token_nonce IS NULL)
    )
);

CREATE UNIQUE INDEX mcp_oauth_clients_org_issuer_key
    ON mcp_oauth_clients (org_id, issuer) WHERE org_id IS NOT NULL;

CREATE UNIQUE INDEX mcp_oauth_clients_shared_issuer_key
    ON mcp_oauth_clients (issuer) WHERE org_id IS NULL;

ALTER TABLE mcp_oauth_clients ENABLE ROW LEVEL SECURITY;
ALTER TABLE mcp_oauth_clients FORCE ROW LEVEL SECURITY;

CREATE POLICY mcp_oauth_clients_select ON mcp_oauth_clients
    FOR SELECT TO PUBLIC
    USING (org_id IS NULL OR app_user_is_member(org_id));

CREATE POLICY mcp_oauth_clients_insert ON mcp_oauth_clients
    FOR INSERT TO PUBLIC
    WITH CHECK (org_id IS NOT NULL AND app_user_is_member(org_id));

CREATE POLICY mcp_oauth_clients_update ON mcp_oauth_clients
    FOR UPDATE TO PUBLIC
    USING      (org_id IS NOT NULL AND app_user_is_member(org_id))
    WITH CHECK (org_id IS NOT NULL AND app_user_is_member(org_id));

CREATE POLICY mcp_oauth_clients_delete ON mcp_oauth_clients
    FOR DELETE TO PUBLIC
    USING (org_id IS NOT NULL AND app_user_is_member(org_id));
