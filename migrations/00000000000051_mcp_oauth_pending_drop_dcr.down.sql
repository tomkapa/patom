-- Re-add the DCR-client columns dropped in migration 51, mirroring the
-- shape migration 50 introduced (see
-- migrations/00000000000050_mcp_oauth_refactor.up.sql:46-84). Existing
-- in-flight pending rows after rollback will not have these columns
-- populated; the pre-this-PR start handler would refuse to proceed and
-- redirect with `status=state_expired`, which is the acceptable failure
-- mode (the TTL is short).

ALTER TABLE mcp_oauth_pending
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
    ADD CONSTRAINT mcp_oauth_pending_dcr_all_or_none CHECK (
        (dcr_client_id IS NULL) = (dcr_token_endpoint_auth_method IS NULL)
        AND (dcr_client_id IS NULL) = (dcr_authorization_endpoint IS NULL)
        AND (dcr_client_id IS NULL) = (dcr_token_endpoint IS NULL)
    );
