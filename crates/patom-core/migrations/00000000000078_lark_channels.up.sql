-- Lark adapter — the Lark-chat <-> Patom-channel mapping (analogue of
-- slack_channels).
--
-- A Lark group chat is mirrored to a normal multi-participant Patom `channels`
-- row so the conversation is a CHANNEL thread, not a two-party DM: every
-- observed human (shadow) is added to the channel's `channel_members`, so their
-- mirrored `posted` rows pass the membership-based RLS the thread feed enforces
-- (a DM-visibility thread would exclude a third participant). The bridge
-- get-or-creates the mapping on the first inbound event from a chat.

CREATE TABLE lark_channels (
    org_id      UUID NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    tenant_key  TEXT NOT NULL CHECK (octet_length(tenant_key) BETWEEN 1 AND 128),
    chat_id     TEXT NOT NULL CHECK (octet_length(chat_id) BETWEEN 1 AND 128),
    channel_id  UUID NOT NULL REFERENCES channels(id) ON DELETE CASCADE,
    created_at  TIMESTAMPTZ NOT NULL,
    PRIMARY KEY (org_id, tenant_key, chat_id)
);

ALTER TABLE lark_channels ENABLE ROW LEVEL SECURITY;
ALTER TABLE lark_channels FORCE ROW LEVEL SECURITY;
CREATE POLICY lark_channels_org_isolation ON lark_channels
    FOR ALL TO PUBLIC
    USING      (app_user_is_member(org_id))
    WITH CHECK (app_user_is_member(org_id));
