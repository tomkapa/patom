-- Slack adapter — Phase 2 (channel-backed threads, GitHub issue #41).
--
-- Maps a Slack channel `(team, slack_channel_id)` to a Patom `channels`
-- row so a Slack-rooted conversation becomes a normal multi-participant
-- channel thread (multiple humans + multiple agents) instead of a
-- two-party DM thread. The bridge get-or-creates the mapping on the first
-- inbound event in a Slack channel and adds each acting linked human to
-- the Patom channel's `channel_members`.
--
-- Webhook events arrive `(team, channel)`-first with no Principal, so the
-- privileged bridge lookup keys on `(org_id, team_id, slack_channel_id)`.

CREATE TABLE slack_channels (
    org_id           UUID NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    team_id          TEXT NOT NULL,
    slack_channel_id TEXT NOT NULL
                     CHECK (octet_length(slack_channel_id) BETWEEN 1 AND 32),
    -- The Patom channel this Slack channel is mirrored into. Composite FK
    -- keeps the mapped channel in the same org as the workspace.
    channel_id       UUID NOT NULL,
    created_at       TIMESTAMPTZ NOT NULL,
    PRIMARY KEY (org_id, team_id, slack_channel_id),
    FOREIGN KEY (org_id, team_id) REFERENCES slack_workspaces(org_id, team_id)
        ON DELETE CASCADE,
    FOREIGN KEY (channel_id, org_id) REFERENCES channels(id, org_id)
        ON DELETE CASCADE
);

-- One Patom channel per Slack channel (and vice versa): the reverse index
-- is UNIQUE so a Patom channel is mirrored from at most one Slack channel.
CREATE UNIQUE INDEX slack_channels_channel_idx ON slack_channels(channel_id);

ALTER TABLE slack_channels ENABLE ROW LEVEL SECURITY;
ALTER TABLE slack_channels FORCE ROW LEVEL SECURITY;
CREATE POLICY slack_channels_org_isolation ON slack_channels
    FOR ALL TO PUBLIC
    USING      (app_user_is_member(org_id))
    WITH CHECK (app_user_is_member(org_id));
