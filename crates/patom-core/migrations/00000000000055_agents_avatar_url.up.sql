-- Per-agent avatar: nullable column on agents, mirroring the 2048-octet
-- cap on users.avatar_url (migration 14) and organizations.avatar_url
-- (migration 48). NULL means "Slack falls back to the app default bot
-- avatar and the FE renders the name monogram"; non-NULL is the public
-- assets-origin URL returned by /api/uploads/agent-avatar/{agent_id} and
-- surfaced as the Slack `icon_url` on outbound agent posts (issue #43).

ALTER TABLE agents
    ADD COLUMN avatar_url TEXT
        CHECK (avatar_url IS NULL OR octet_length(avatar_url) <= 2048);
