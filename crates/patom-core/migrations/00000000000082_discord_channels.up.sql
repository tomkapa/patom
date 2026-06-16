-- Discord adapter — the Discord-channel <-> Patom-channel mapping (analogue of
-- lark_channels / slack_channels).
--
-- A Discord channel is mirrored to a normal multi-participant Patom `channels`
-- row so the conversation is a CHANNEL thread, not a two-party DM: every
-- observed human (shadow) is added to the channel's `channel_members`, so their
-- mirrored `posted` rows pass the membership-based RLS the thread feed enforces
-- (a DM-visibility thread would exclude a third participant). The bridge
-- get-or-creates the mapping on the first inbound event from a channel.
--
-- The key is the grouping channel: for a message in a Discord thread, that is
-- the thread's PARENT channel (so every thread under one channel shares one
-- Patom channel); for a top-level message it is the channel itself. A DM keys on
-- the DM channel id.

CREATE TABLE discord_channels (
    org_id             UUID NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    guild_id           TEXT NOT NULL CHECK (octet_length(guild_id) BETWEEN 1 AND 32),
    discord_channel_id TEXT NOT NULL CHECK (octet_length(discord_channel_id) BETWEEN 1 AND 32),
    channel_id         UUID NOT NULL REFERENCES channels(id) ON DELETE CASCADE,
    created_at         TIMESTAMPTZ NOT NULL,
    PRIMARY KEY (org_id, guild_id, discord_channel_id)
);

-- One Patom channel mirrors exactly one Discord channel (the bridge derives a
-- slug per discord_channel_id and never reuses a channel). Enforce that 1:1
-- mapping, mirroring lark_channels' UNIQUE(channel_id) — channels.id is a global
-- UUID, so a bare channel_id index is org-safe.
CREATE UNIQUE INDEX discord_channels_channel_idx ON discord_channels (channel_id);

ALTER TABLE discord_channels ENABLE ROW LEVEL SECURITY;
ALTER TABLE discord_channels FORCE ROW LEVEL SECURITY;
CREATE POLICY discord_channels_org_isolation ON discord_channels
    FOR ALL TO PUBLIC
    USING      (app_user_is_member(org_id))
    WITH CHECK (app_user_is_member(org_id));
