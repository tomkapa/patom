-- MCP credentials seam (R2 — phase B of the catalog/credentials/OAuth bundle).
--
-- Splits sensitive header / token material out of `mcp_servers.config` JSONB
-- (where it was plaintext, visible in DB dumps and every list/read response)
-- into a separate, RLS-scoped, envelope-encrypted table. The encryption is
-- AES-256-GCM with per-org KEKs derived via HKDF-SHA256 from a process-wide
-- master KEK (`PATOM_MASTER_KEK`); see `src/crypto`.
--
-- One row per server, optional: a server may have no credentials (a public
-- MCP endpoint) — then no row exists. `kind` distinguishes two payload
-- shapes that share the same envelope:
--
--   - 'static_headers'  → JSON `{ "headers": {<name>: <value>, …} }`
--   - 'oauth2'          → JSON `{ "access_token", "refresh_token",
--                                  "expires_at", "scope", "issuer",
--                                  "token_endpoint" }` (populated in phase C)
--
-- Pre-launch: NOT NULL with no backfill (see `feedback_no_backcompat`).
-- Existing dev rows must be wiped before applying.

CREATE TABLE mcp_server_credentials (
    server_id   UUID PRIMARY KEY REFERENCES mcp_servers(id) ON DELETE CASCADE,
    org_id      UUID NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    kind        TEXT NOT NULL CHECK (kind IN ('static_headers','oauth2')),
    ciphertext  BYTEA NOT NULL,
    nonce       BYTEA NOT NULL CHECK (octet_length(nonce) = 12),
    key_version SMALLINT NOT NULL DEFAULT 1,
    created_at  TIMESTAMPTZ NOT NULL,
    updated_at  TIMESTAMPTZ NOT NULL
);

CREATE INDEX mcp_server_credentials_org_idx
    ON mcp_server_credentials (org_id);

ALTER TABLE mcp_server_credentials ENABLE ROW LEVEL SECURITY;
ALTER TABLE mcp_server_credentials FORCE ROW LEVEL SECURITY;
CREATE POLICY mcp_server_credentials_org_isolation ON mcp_server_credentials
    FOR ALL TO PUBLIC
    USING      (app_user_is_member(org_id))
    WITH CHECK (app_user_is_member(org_id));

-- Trigger: keep `mcp_server_credentials.org_id` in lockstep with
-- `mcp_servers.org_id`. A CHECK can't reference another row, so the trigger
-- enforces the invariant on every insert/update. This mirrors the pattern
-- used by `scheduled_tasks` in migration 19.
CREATE OR REPLACE FUNCTION enforce_mcp_credentials_parent_org() RETURNS TRIGGER
    LANGUAGE plpgsql AS $$
DECLARE
    parent_org UUID;
BEGIN
    SELECT org_id INTO parent_org FROM mcp_servers WHERE id = NEW.server_id;
    IF parent_org IS NULL THEN
        RAISE EXCEPTION 'mcp_server_credentials: parent server % does not exist', NEW.server_id;
    END IF;
    IF parent_org <> NEW.org_id THEN
        RAISE EXCEPTION
            'mcp_server_credentials: org_id % does not match parent server org_id %',
            NEW.org_id, parent_org;
    END IF;
    RETURN NEW;
END
$$;

CREATE TRIGGER mcp_server_credentials_enforce_org
    BEFORE INSERT OR UPDATE OF org_id, server_id ON mcp_server_credentials
    FOR EACH ROW
    EXECUTE FUNCTION enforce_mcp_credentials_parent_org();
