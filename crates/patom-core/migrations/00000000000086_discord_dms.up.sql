-- Discord adapter — the agent↔human DM binding (issue #178, arm 3).
--
-- A Discord DM channel is opened on demand (`POST /users/@me/channels`) and is
-- NOT a guild container, so it cannot live in `discord_threads` (whose PK
-- requires a NOT NULL guild_id). This dedicated table maps a Patom DM thread to
-- the bot's DM channel snowflake so subsequent turns post to the same channel
-- without re-opening it.
--
--   outbound: the router looks up by patom_thread_id — where to post — so that
--             column is UNIQUE (which also supplies its index). The lookup runs
--             BEFORE opening a new DM channel, so a re-fire never opens a second.
--
-- application_id pins the DM to the bot that owns it (whose token posts).

CREATE TABLE discord_dms (
    org_id          UUID NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    application_id  TEXT NOT NULL CHECK (octet_length(application_id) BETWEEN 1 AND 32),
    patom_thread_id UUID NOT NULL UNIQUE REFERENCES threads(id) ON DELETE CASCADE,
    dm_channel_id   TEXT NOT NULL CHECK (octet_length(dm_channel_id) BETWEEN 1 AND 32),
    created_at      TIMESTAMPTZ NOT NULL,
    PRIMARY KEY (org_id, application_id, patom_thread_id),
    FOREIGN KEY (org_id, application_id) REFERENCES discord_apps (org_id, application_id) ON DELETE CASCADE
);

ALTER TABLE discord_dms ENABLE ROW LEVEL SECURITY;
ALTER TABLE discord_dms FORCE ROW LEVEL SECURITY;
CREATE POLICY discord_dms_org_isolation ON discord_dms
    FOR ALL TO PUBLIC
    USING      (app_user_is_member(org_id))
    WITH CHECK (app_user_is_member(org_id));
