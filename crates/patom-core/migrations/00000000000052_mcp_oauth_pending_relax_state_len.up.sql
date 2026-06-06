-- Relax the `mcp_oauth_pending.state` length check.
--
-- The original constraint (migration 22) required 32-128 bytes — that
-- matched patom's hand-rolled `oauth2::CsrfToken::new_random_len(32)`
-- in the old `build_authorize_url`. After adopting
-- `rmcp::transport::auth` (migration 51), rmcp's
-- `AuthorizationManager::get_authorization_url` mints the CSRF token
-- via the `oauth2` crate's default `CsrfToken::new_random` — 16 bytes
-- of randomness, base64url-encoded to 22 chars. That's still
-- cryptographically sufficient (128 bits of entropy) but trips the
-- old lower bound. `oauth_login_states` keeps the 32-byte floor;
-- only the MCP-OAuth side flips the rmcp-generated token.

ALTER TABLE mcp_oauth_pending
    DROP CONSTRAINT mcp_oauth_pending_state_check,
    ADD CONSTRAINT mcp_oauth_pending_state_check
        CHECK (octet_length(state) BETWEEN 16 AND 128);
