-- Upstream MCP OAuth (R3 — phase C of the catalog/credentials/OAuth bundle).
--
-- Three pieces:
--   1. `mcp_servers.connection_status` — a tri-state surfaced on every
--      list/read so the UI can render "ok" / "reconnect required" / "error"
--      without parsing `last_error` strings.
--   2. `mcp_oauth_clients` — one row per (org, issuer); stores the
--      RFC 7591 Dynamic Client Registration output (client_id +
--      encrypted client_secret + encrypted registration_access_token).
--      Per-org so dynamic registrations across tenants don't share state;
--      the issuer column is the AS root identifier from RFC 8414.
--   3. `mcp_oauth_pending` — short-lived rows that hold the PKCE verifier
--      and CSRF state between `POST /oauth/start` and `GET /oauth/callback`.
--      Not RLS'd: the callback runs without a session (the browser came
--      back from the vendor); `state` is the gate.
--
-- Pre-launch: no backfill (see `feedback_no_backcompat`).

ALTER TABLE mcp_servers
    ADD COLUMN connection_status TEXT NOT NULL DEFAULT 'ok'
        CHECK (connection_status IN ('ok', 'reconnect_required', 'error'));

CREATE TABLE mcp_oauth_clients (
    org_id     UUID NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    issuer     TEXT NOT NULL CHECK (octet_length(issuer) BETWEEN 1 AND 2048),
    client_id  TEXT NOT NULL CHECK (octet_length(client_id) BETWEEN 1 AND 1024),
    -- Authorization / Token / Registration endpoints discovered at register
    -- time; cached on the row so the OAuth flow never has to re-discover
    -- the AS metadata mid-flight.
    authorization_endpoint TEXT NOT NULL CHECK (octet_length(authorization_endpoint) BETWEEN 1 AND 2048),
    token_endpoint         TEXT NOT NULL CHECK (octet_length(token_endpoint) BETWEEN 1 AND 2048),
    -- Optional: only present when the AS supports incremental client
    -- registration management (RFC 7592). We don't drive a delete today;
    -- it's stored so a future cleanup task can deregister.
    registration_client_uri              TEXT CHECK (registration_client_uri IS NULL OR octet_length(registration_client_uri) <= 2048),
    registration_access_token_ciphertext BYTEA,
    registration_access_token_nonce      BYTEA CHECK (
        registration_access_token_nonce IS NULL OR octet_length(registration_access_token_nonce) = 12
    ),
    -- Confidential clients have a client_secret; public clients (PKCE-only)
    -- store NULL here.
    client_secret_ciphertext BYTEA,
    client_secret_nonce      BYTEA CHECK (
        client_secret_nonce IS NULL OR octet_length(client_secret_nonce) = 12
    ),
    key_version SMALLINT NOT NULL DEFAULT 1,
    token_endpoint_auth_method TEXT NOT NULL
        CHECK (token_endpoint_auth_method IN ('none','client_secret_basic','client_secret_post')),
    scope       TEXT CHECK (scope IS NULL OR octet_length(scope) <= 2048),
    created_at  TIMESTAMPTZ NOT NULL,
    PRIMARY KEY (org_id, issuer),
    -- Pair-or-null: secret_ciphertext and secret_nonce both present, both
    -- absent, never one without the other.
    CHECK (
        (client_secret_ciphertext IS NULL) = (client_secret_nonce IS NULL)
    ),
    CHECK (
        (registration_access_token_ciphertext IS NULL)
        = (registration_access_token_nonce IS NULL)
    )
);

ALTER TABLE mcp_oauth_clients ENABLE ROW LEVEL SECURITY;
ALTER TABLE mcp_oauth_clients FORCE ROW LEVEL SECURITY;
CREATE POLICY mcp_oauth_clients_org_isolation ON mcp_oauth_clients
    FOR ALL TO PUBLIC
    USING      (app_user_is_member(org_id))
    WITH CHECK (app_user_is_member(org_id));

-- Pending row: holds the PKCE verifier + state for the callback. Not
-- RLS'd because the callback request has no `app.user_id` GUC set —
-- the user just returned from the vendor's consent screen and may not
-- have an active session. The `state` column is the CSRF gate, and the
-- callback handler enforces `expires_at > now()` plus a one-shot
-- delete-after-read so a replay can't reuse the row.
CREATE TABLE mcp_oauth_pending (
    state         TEXT PRIMARY KEY
                  CHECK (octet_length(state) BETWEEN 32 AND 128),
    server_id     UUID NOT NULL REFERENCES mcp_servers(id) ON DELETE CASCADE,
    user_id       UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    org_id        UUID NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    issuer        TEXT NOT NULL CHECK (octet_length(issuer) BETWEEN 1 AND 2048),
    pkce_verifier TEXT NOT NULL
                  CHECK (octet_length(pkce_verifier) BETWEEN 32 AND 128),
    redirect_to   TEXT CHECK (redirect_to IS NULL OR octet_length(redirect_to) <= 2048),
    created_at    TIMESTAMPTZ NOT NULL,
    expires_at    TIMESTAMPTZ NOT NULL
);
CREATE INDEX mcp_oauth_pending_expires_idx ON mcp_oauth_pending(expires_at);
