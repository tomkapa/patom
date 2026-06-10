-- BYO-OAuth for custom MCP connectors.
--
-- A custom (operator-supplied URL) catalog entry may now authenticate via
-- OAuth using the operator's *own* client credentials — a fourth
-- `client_source = 'user_supplied'`. The client identity is connector-level,
-- so it lives on the org-scoped `mcp_catalog` row (1:1 with the connector,
-- already loaded in both the OAuth start and callback paths). It is NOT put
-- in `mcp_server_credentials`, whose row is overwritten with the access
-- token at callback — that would clobber the secret.
--
-- The `client_id` is public (plaintext). The `client_secret` is optional
-- (confidential clients supply it; public/PKCE clients omit it) and is
-- sealed with the org KEK using the same `(ciphertext, nonce, key_version)`
-- envelope as `mcp_server_credentials`.
--
-- Additive nullable columns → safe on the live DB, no backfill
-- (CLAUDE.md §14).

-- Widen the client_source domain. Inline column CHECK from migration 50 is
-- auto-named `mcp_catalog_client_source_check`; drop + re-add the 4-value form.
ALTER TABLE mcp_catalog
    DROP CONSTRAINT mcp_catalog_client_source_check;

ALTER TABLE mcp_catalog
    ADD CONSTRAINT mcp_catalog_client_source_check
        CHECK (client_source IN ('platform','dcr','none','user_supplied'));

-- Connector-level OAuth client material. `oauth_client_id` plaintext (it is
-- not a secret); the secret triple mirrors `mcp_server_credentials`.
ALTER TABLE mcp_catalog
    ADD COLUMN oauth_client_id TEXT
        CHECK (oauth_client_id IS NULL OR octet_length(oauth_client_id) BETWEEN 1 AND 512),
    ADD COLUMN oauth_client_secret_ciphertext BYTEA,
    ADD COLUMN oauth_client_secret_nonce BYTEA
        CHECK (
            oauth_client_secret_nonce IS NULL OR octet_length(oauth_client_secret_nonce) = 12
        ),
    ADD COLUMN oauth_client_secret_key_version SMALLINT,
    -- The sealed-secret triple is all-or-none: a public client has the id
    -- with no secret; a confidential client has all three. A partial row
    -- would trip the Rust decoder's exhaustive match (CLAUDE.md §6).
    ADD CONSTRAINT mcp_catalog_oauth_secret_triple CHECK (
        (oauth_client_secret_ciphertext IS NULL) = (oauth_client_secret_nonce IS NULL)
        AND (oauth_client_secret_ciphertext IS NULL) = (oauth_client_secret_key_version IS NULL)
    ),
    -- A user-supplied OAuth connector must carry a client_id; the secret is
    -- optional (public/PKCE). Mirrors the Rust newtype invariant so a manual
    -- DB edit can't produce an unauthenticatable row.
    ADD CONSTRAINT mcp_catalog_user_supplied_requires_client_id CHECK (
        client_source <> 'user_supplied' OR oauth_client_id IS NOT NULL
    );
