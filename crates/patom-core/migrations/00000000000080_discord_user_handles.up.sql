-- Discord adapter — the persistent global people directory (design doc §4).
--
-- One stable Patom identity per Discord user per org — the same across every
-- channel and thread. Every observed sender (and every roster member, incl.
-- silent ones who never post) materializes here, backed by a shadow colleague
-- until/unless the person becomes a real Patom user (the shadow→real merge is
-- deferred, §9b).
--
-- PK on (org_id, discord_user_id): Discord's user snowflake is GLOBAL (not
-- tenant-scoped like Lark's user_id), so the natural key is (org, user) — one
-- shadow per person per org, uniform across events, roster, and history. No
-- per-bot satellite handle is needed (the snowflake itself is the @-tag id).
-- colleague_id is the durable join target; the FK is on colleague_id alone
-- (colleagues has no org-composite unique to target), matching lark_user_handles
-- — the colleague's own org_id is enforced by the mint trigger writing
-- NEW.org_id.

CREATE TABLE discord_user_handles (
    org_id           UUID NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    discord_user_id  TEXT NOT NULL CHECK (octet_length(discord_user_id) BETWEEN 1 AND 32),
    -- The Discord display name observed for this user (nick > global_name >
    -- username, resolved adapter-side). Mirrored onto the shadow user's
    -- display_name; refreshed opportunistically. NULL if Discord gave none.
    display_name     TEXT NULL CHECK (display_name IS NULL OR octet_length(display_name) BETWEEN 1 AND 256),
    colleague_id     UUID NOT NULL REFERENCES colleagues(id) ON DELETE CASCADE,
    created_at       TIMESTAMPTZ NOT NULL,
    PRIMARY KEY (org_id, discord_user_id)
);

-- The poster's outbound mention resolve is colleague_id -> discord_user_id; the
-- reverse lookup needs an index so it is a point read.
CREATE INDEX discord_user_handles_colleague_idx ON discord_user_handles (org_id, colleague_id);

ALTER TABLE discord_user_handles ENABLE ROW LEVEL SECURITY;
ALTER TABLE discord_user_handles FORCE ROW LEVEL SECURITY;
CREATE POLICY discord_user_handles_org_isolation ON discord_user_handles
    FOR ALL TO PUBLIC
    USING      (app_user_is_member(org_id))
    WITH CHECK (app_user_is_member(org_id));
