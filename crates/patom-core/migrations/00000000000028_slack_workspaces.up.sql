-- Slack adapter — Phase 1 (workspace install).
--
-- One row per (Patom org, Slack team). Holds the bot user id (so the
-- bridge can ignore the bot's own messages) and the envelope-encrypted
-- bot token (xoxb-…). Crypto envelope mirrors `mcp_oauth_clients` —
-- (ciphertext, nonce, key_version) triple sealed by `OrgEncryptor`.
--
-- Webhook events from Slack arrive `team_id`-first with no Principal
-- attached, so the privileged lookup path needs `team_id` to be
-- uniquely addressable on its own; the unique index below makes that
-- safe. The composite PK still anchors RLS on `org_id`.

CREATE TABLE slack_workspaces (
    org_id              UUID NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    team_id             TEXT NOT NULL
                        CHECK (octet_length(team_id) BETWEEN 1 AND 32),
    team_name           TEXT NOT NULL
                        CHECK (octet_length(team_name) BETWEEN 1 AND 256),
    bot_user_id         TEXT NOT NULL
                        CHECK (octet_length(bot_user_id) BETWEEN 1 AND 32),
    -- AES-GCM ciphertext + 12-byte nonce, sealed by OrgEncryptor at the
    -- org's KEK. Mirrors `mcp_oauth_clients.client_secret_*`.
    bot_token_ciphertext BYTEA NOT NULL,
    bot_token_nonce      BYTEA NOT NULL
                        CHECK (octet_length(bot_token_nonce) = 12),
    key_version         SMALLINT NOT NULL DEFAULT 1,
    -- Space-separated OAuth scopes granted at install (e.g.
    -- "app_mentions:read chat:write chat:write.customize"). Stored so
    -- reinstall flows can detect missing scopes without re-fetching.
    scopes              TEXT NOT NULL
                        CHECK (octet_length(scopes) <= 2048),
    installed_by_user_id UUID NOT NULL REFERENCES users(id),
    installed_at        TIMESTAMPTZ NOT NULL,
    PRIMARY KEY (org_id, team_id)
);

-- Reverse-lookup index: webhook handler has only `team_id` and must
-- resolve to `org_id`. UNIQUE because a single Slack workspace can
-- install into exactly one Patom org (re-install in a different org
-- requires uninstall first).
CREATE UNIQUE INDEX slack_workspaces_team_idx ON slack_workspaces(team_id);

ALTER TABLE slack_workspaces ENABLE ROW LEVEL SECURITY;
ALTER TABLE slack_workspaces FORCE ROW LEVEL SECURITY;
CREATE POLICY slack_workspaces_org_isolation ON slack_workspaces
    FOR ALL TO PUBLIC
    USING      (app_user_is_member(org_id))
    WITH CHECK (app_user_is_member(org_id));
