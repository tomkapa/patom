-- Drop the DCR-client columns added in migration 50.
--
-- Migration 50 carried freshly-registered DCR client material (client_id,
-- encrypted client_secret, auth method, endpoints) on `mcp_oauth_pending`
-- so it could ride from start → callback without a separate clients
-- table. After adopting `rmcp::transport::auth` (see commit message),
-- rmcp's `CredentialStore::save` persists the DCR-issued client_id
-- directly inside `mcp_server_credentials.OAuth2Payload.stored.client_id`
-- (i.e. `rmcp::transport::auth::StoredCredentials.client_id`). The
-- secret is no longer persisted at all — public-client semantics fall
-- out of rmcp's PKCE+DCR-with-no-secret choice. The four endpoint
-- columns are also gone: rmcp re-runs discovery on every
-- `AuthorizationManager::initialize_from_store`.
--
-- `pkce_verifier` stays — `rmcp::StateStore::save` writes it and
-- `rmcp::StateStore::load` reads it on callback. Patom's `state_adapter`
-- module persists it in that column.

ALTER TABLE mcp_oauth_pending
    DROP CONSTRAINT mcp_oauth_pending_dcr_secret_pair,
    DROP CONSTRAINT mcp_oauth_pending_dcr_all_or_none,
    DROP COLUMN dcr_client_id,
    DROP COLUMN dcr_client_secret_ciphertext,
    DROP COLUMN dcr_client_secret_nonce,
    DROP COLUMN dcr_token_endpoint_auth_method,
    DROP COLUMN dcr_authorization_endpoint,
    DROP COLUMN dcr_token_endpoint;
