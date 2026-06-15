-- Lark adapter — BYO-bot registration. One row per (Patom org, Lark app).
--
-- A Lark "app" == one bot identity == one agent (the multi-BYO-bot topology):
-- N rows per org, each pinned to an agent. Events arrive over the
-- long-connection keyed by app_id with NO Principal attached, so app_id must be
-- uniquely addressable on its own (the UNIQUE index below); the composite PK
-- still anchors RLS on org_id.
--
-- The app_secret is envelope-encrypted by OrgEncryptor (AES-256-GCM under the
-- per-org KEK) — (ciphertext, nonce, key_version) triple, mirroring
-- slack_workspaces.bot_token_* / mcp_oauth_clients. tenant_key is the Lark
-- tenant the app is installed into (the identity scope_id, §7 of the design
-- doc); it is resolved at first connect, so NULLABLE until then.

CREATE TABLE lark_apps (
    org_id                UUID NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    app_id                TEXT NOT NULL
                          CHECK (octet_length(app_id) BETWEEN 1 AND 128),
    -- The Patom agent this bot speaks as. RESTRICT (the default): deleting an
    -- agent with a live bot is an operator error to surface, not a silent
    -- teardown of a connected bot.
    agent_id              UUID NOT NULL REFERENCES agents(id),
    -- AES-256-GCM ciphertext + 12-byte nonce of the Lark app_secret, sealed by
    -- OrgEncryptor at the org KEK. Mirrors slack_workspaces.bot_token_*.
    app_secret_ciphertext BYTEA NOT NULL,
    app_secret_nonce      BYTEA NOT NULL
                          CHECK (octet_length(app_secret_nonce) = 12),
    key_version           SMALLINT NOT NULL DEFAULT 1,
    -- Resolved at first token mint / handshake. NULLABLE until first connect.
    tenant_key            TEXT NULL
                          CHECK (tenant_key IS NULL OR octet_length(tenant_key) BETWEEN 1 AND 128),
    created_at            TIMESTAMPTZ NOT NULL,
    PRIMARY KEY (org_id, app_id)
);

-- Reverse-lookup index: the WS manager / token provider has only app_id and
-- must resolve org_id + agent + secret. UNIQUE because a single Lark app
-- installs into exactly one Patom org.
CREATE UNIQUE INDEX lark_apps_app_idx ON lark_apps (app_id);

ALTER TABLE lark_apps ENABLE ROW LEVEL SECURITY;
ALTER TABLE lark_apps FORCE ROW LEVEL SECURITY;
CREATE POLICY lark_apps_org_isolation ON lark_apps
    FOR ALL TO PUBLIC
    USING      (app_user_is_member(org_id))
    WITH CHECK (app_user_is_member(org_id));
