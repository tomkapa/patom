-- Discord adapter — BYO-bot registration. One row per (Patom org, Discord app).
--
-- A Discord "application" == one bot identity == one agent (the multi-BYO-bot
-- topology): N rows per org, each pinned to an agent. Gateway events arrive over
-- the WebSocket keyed by application_id with NO Principal attached, so
-- application_id must be uniquely addressable on its own (the UNIQUE index
-- below); the composite PK still anchors RLS on org_id.
--
-- The bot token is envelope-encrypted by OrgEncryptor (AES-256-GCM under the
-- per-org KEK) — (ciphertext, nonce, key_version) triple, mirroring
-- lark_apps.app_secret_* / slack_workspaces.bot_token_*. Unlike Lark's
-- tenant_access_token, a Discord bot token is STATIC (no expiry, no refresh) —
-- it is only re-credentialed on reset/leak. bot_user_id is the bot's own user
-- snowflake, learned from the gateway READY payload (so the bridge can drop the
-- bot's own re-delivered messages); NULL until first connect.

CREATE TABLE discord_apps (
    org_id                UUID NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    application_id        TEXT NOT NULL
                          CHECK (octet_length(application_id) BETWEEN 1 AND 32),
    -- The Patom agent this bot speaks as. RESTRICT (the default): deleting an
    -- agent with a live bot is an operator error to surface, not a silent
    -- teardown of a connected bot.
    agent_id              UUID NOT NULL REFERENCES agents(id),
    -- AES-256-GCM ciphertext + 12-byte nonce of the Discord bot token, sealed by
    -- OrgEncryptor at the org KEK. Mirrors lark_apps.app_secret_*.
    bot_token_ciphertext  BYTEA NOT NULL,
    bot_token_nonce       BYTEA NOT NULL
                          CHECK (octet_length(bot_token_nonce) = 12),
    key_version           SMALLINT NOT NULL DEFAULT 1,
    -- The bot's own user snowflake, resolved from the gateway READY event.
    -- NULLABLE until first connect.
    bot_user_id           TEXT NULL
                          CHECK (bot_user_id IS NULL OR octet_length(bot_user_id) BETWEEN 1 AND 32),
    created_at            TIMESTAMPTZ NOT NULL,
    PRIMARY KEY (org_id, application_id),
    -- One bot per agent within an org (the doc invariant "one self-built Discord
    -- application per agent"). Without this, `app_id_for_agent`'s LIMIT 1 would
    -- silently pick an arbitrary row instead of enforcing the invariant.
    UNIQUE (org_id, agent_id)
);

-- Reverse-lookup index: the gateway manager / bridge has only application_id and
-- must resolve org_id + agent + token. UNIQUE because a single Discord app
-- installs into exactly one Patom org.
CREATE UNIQUE INDEX discord_apps_app_idx ON discord_apps (application_id);

ALTER TABLE discord_apps ENABLE ROW LEVEL SECURITY;
ALTER TABLE discord_apps FORCE ROW LEVEL SECURITY;
CREATE POLICY discord_apps_org_isolation ON discord_apps
    FOR ALL TO PUBLIC
    USING      (app_user_is_member(org_id))
    WITH CHECK (app_user_is_member(org_id));
