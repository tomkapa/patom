-- Agent channel membership (issue #178).
--
-- `channel_members` is human-only (user_id-keyed). Channel-posting authority for
-- agents needs the same "is this colleague a member?" check, but a colleague can
-- be an agent (no user_id). Rather than reshape the live human table (and its
-- `ON CONFLICT (channel_id, user_id)` writers on every chat surface), agents get
-- a sibling table keyed by their colleague id. The membership predicate is the
-- UNION of the two.
--
-- How agents gain membership: the bot's presence in a mirrored external chat IS
-- the membership. Roster sync writes a row on bot-join; this migration backfills
-- existing Lark/Discord-mirrored channels from the bot↔channel bindings already
-- recorded (a thread binding proves the bot operated in that channel).

CREATE TABLE channel_agent_members (
    channel_id   UUID NOT NULL,
    colleague_id UUID NOT NULL REFERENCES colleagues(id) ON DELETE CASCADE,
    org_id       UUID NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    added_at     TIMESTAMPTZ NOT NULL,
    PRIMARY KEY (channel_id, colleague_id),
    FOREIGN KEY (channel_id, org_id) REFERENCES channels (id, org_id) ON DELETE CASCADE
);
CREATE INDEX channel_agent_members_colleague_idx ON channel_agent_members (colleague_id);
CREATE INDEX channel_agent_members_org_idx ON channel_agent_members (org_id);

-- Backfill: a Lark thread binding ties (app → agent) to a (tenant_key, chat_id)
-- that lark_channels maps to a Patom channel — proof the bot is in that channel.
INSERT INTO channel_agent_members (channel_id, colleague_id, org_id, added_at)
SELECT DISTINCT lc.channel_id, ac.id, lc.org_id, now()
  FROM lark_threads lt
  JOIN lark_channels lc
    ON lc.org_id = lt.org_id AND lc.tenant_key = lt.tenant_key AND lc.chat_id = lt.chat_id
  JOIN lark_apps la ON la.org_id = lt.org_id AND la.app_id = lt.app_id
  JOIN colleagues ac ON ac.org_id = lt.org_id AND ac.kind = 'agent' AND ac.agent_id = la.agent_id
ON CONFLICT DO NOTHING;

-- Discord: a thread binding's container (or its parent channel, for a thread)
-- maps to a discord_channels row; the app → agent.
INSERT INTO channel_agent_members (channel_id, colleague_id, org_id, added_at)
SELECT DISTINCT dc.channel_id, ac.id, dc.org_id, now()
  FROM discord_threads dt
  JOIN discord_channels dc
    ON dc.org_id = dt.org_id AND dc.guild_id = dt.guild_id
   AND dc.discord_channel_id = COALESCE(dt.parent_id, dt.container_id)
  JOIN discord_apps da ON da.org_id = dt.org_id AND da.application_id = dt.application_id
  JOIN colleagues ac ON ac.org_id = dt.org_id AND ac.kind = 'agent' AND ac.agent_id = da.agent_id
ON CONFLICT DO NOTHING;

ALTER TABLE channel_agent_members ENABLE ROW LEVEL SECURITY;
ALTER TABLE channel_agent_members FORCE ROW LEVEL SECURITY;
CREATE POLICY channel_agent_members_org_isolation ON channel_agent_members
    FOR ALL TO PUBLIC
    USING      (app_user_is_member(org_id))
    WITH CHECK (app_user_is_member(org_id));
