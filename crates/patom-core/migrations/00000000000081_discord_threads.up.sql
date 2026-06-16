-- Discord adapter — the Discord-container <-> Patom-thread binding (analogue of
-- lark_threads / slack_threads).
--
-- A Discord *thread* IS a channel (same POST /channels/{id}/messages endpoint),
-- so a Patom thread binds to a single CONTAINER snowflake: the channel id for a
-- top-level channel, or the thread id for a thread. There is no thread-vs-channel
-- branching at the post seam.
--
--   inbound:  lookup by (org_id, application_id, guild_id, container_id) — new
--             thread or continuation. The key is scoped to the BOT (org +
--             application), not just the Discord container: multiple bots — even
--             across orgs — can share a guild/channel, and each must bind its OWN
--             Patom thread for that container. A bare (guild_id, container_id) key
--             would let one bot's binding shadow another's (and leak across orgs).
--   outbound: the stream pump looks up by patom_thread_id — where to post — so
--             that column is UNIQUE (which also supplies its index).
--
-- guild_id is the org/tenant anchor; parent_id is recorded as enrichment (the
-- parent channel of a thread, NULL for a top-level channel). application_id pins
-- the thread to the bot whose gateway connection delivered its events, so the
-- outbound poster knows which bot token to use. backfill_complete gates the
-- one-shot pre-join history backfill (design doc §5) so it runs at most once.

CREATE TABLE discord_threads (
    org_id            UUID NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    application_id    TEXT NOT NULL CHECK (octet_length(application_id) BETWEEN 1 AND 32),
    guild_id          TEXT NOT NULL CHECK (octet_length(guild_id) BETWEEN 1 AND 32),
    container_id      TEXT NOT NULL CHECK (octet_length(container_id) BETWEEN 1 AND 32),
    parent_id         TEXT NULL CHECK (parent_id IS NULL OR octet_length(parent_id) BETWEEN 1 AND 32),
    patom_thread_id   UUID NOT NULL UNIQUE REFERENCES threads(id) ON DELETE CASCADE,
    backfill_complete BOOLEAN NOT NULL DEFAULT FALSE,
    created_at        TIMESTAMPTZ NOT NULL,
    PRIMARY KEY (org_id, application_id, guild_id, container_id),
    FOREIGN KEY (org_id, application_id) REFERENCES discord_apps (org_id, application_id) ON DELETE CASCADE
);

ALTER TABLE discord_threads ENABLE ROW LEVEL SECURITY;
ALTER TABLE discord_threads FORCE ROW LEVEL SECURITY;
CREATE POLICY discord_threads_org_isolation ON discord_threads
    FOR ALL TO PUBLIC
    USING      (app_user_is_member(org_id))
    WITH CHECK (app_user_is_member(org_id));
