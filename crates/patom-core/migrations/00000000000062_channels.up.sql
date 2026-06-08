-- User-created channels.
--
-- A channel is an org-scoped, member-gated space that groups human-initiated
-- thread roots. Agents reach every channel by default (there is no agent
-- membership); only humans are members. A thread root carries a nullable
-- `prompt_requests.channel_id`: set => the thread is a channel post; NULL => a
-- direct message with an agent, private to its human creator
-- (`sessions.created_by_user_id`). Only roots are stamped — replies inherit
-- location from the root — so the column stays nullable.
--
-- Permissions live in the app layer: anyone may create a channel; rename /
-- archive / membership changes are restricted to the channel's creator
-- (`created_by_user_id`). The default per-org `#general` channel is
-- system-owned (`created_by_user_id IS NULL`) so it can never be renamed or
-- archived. Archive is soft (`archived_at`); rows are never hard-deleted.

CREATE TABLE channels (
    id                 UUID PRIMARY KEY,
    org_id             UUID NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    -- Mirrors the `ChannelName` newtype: lowercase slug, <= 63 bytes.
    name               TEXT NOT NULL CHECK (name ~ '^[a-z0-9][a-z0-9-]{0,62}$'),
    -- NULL for the system-owned default channel; otherwise the creator.
    created_by_user_id UUID NULL REFERENCES users(id) ON DELETE SET NULL,
    created_at         TIMESTAMPTZ NOT NULL,
    archived_at        TIMESTAMPTZ NULL
);
-- One active channel per (org, name); archiving frees the name for reuse.
-- Doubles as the ON CONFLICT inference target for the seed below.
CREATE UNIQUE INDEX channels_org_name_active_unique
    ON channels (org_id, name) WHERE archived_at IS NULL;
CREATE INDEX channels_org_idx ON channels (org_id);

-- Human members only. `org_id` is carried for the org-isolation RLS policy and
-- the per-user membership scan in the thread-list query.
CREATE TABLE channel_members (
    channel_id UUID NOT NULL REFERENCES channels(id) ON DELETE CASCADE,
    user_id    UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    org_id     UUID NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    added_at   TIMESTAMPTZ NOT NULL,
    PRIMARY KEY (channel_id, user_id)
);
CREATE INDEX channel_members_user_idx ON channel_members (user_id);
CREATE INDEX channel_members_org_idx  ON channel_members (org_id);

ALTER TABLE prompt_requests
    ADD COLUMN channel_id UUID NULL REFERENCES channels(id) ON DELETE SET NULL;
CREATE INDEX prompt_requests_channel_idx
    ON prompt_requests (channel_id) WHERE channel_id IS NOT NULL;

-- ── Backfill BEFORE enabling RLS (app.user_id is unset during migration, so
-- the org-isolation WITH CHECK would otherwise reject these inserts). On a
-- fresh/reset DB these are no-ops. ──

-- One system-owned `#general` per existing org, timestamped from the org (§11:
-- no NOW()). Idempotent via the partial unique index.
INSERT INTO channels (id, org_id, name, created_by_user_id, created_at, archived_at)
SELECT gen_random_uuid(), o.id, 'general', NULL, o.created_at, NULL
  FROM organizations o
ON CONFLICT (org_id, name) WHERE archived_at IS NULL DO NOTHING;

-- Enroll every existing member into their org's `#general`.
INSERT INTO channel_members (channel_id, user_id, org_id, added_at)
SELECT c.id, m.user_id, m.org_id, m.created_at
  FROM org_members m
  JOIN channels c
    ON c.org_id = m.org_id AND c.name = 'general' AND c.archived_at IS NULL
ON CONFLICT (channel_id, user_id) DO NOTHING;

-- ── Full reset of thread data (user-approved). DELETE rather than TRUNCATE so
-- the ON DELETE SET NULL on `agent_memories.source_turn_id` is honored —
-- long-term agent memories survive, only their source-turn link is cleared
-- (TRUNCATE CASCADE would ignore the rule and wipe them). Deleting sessions
-- cascades to prompt_requests, session_messages, tool_calls, turn_metrics,
-- leases, request dags, and session-scoped working memory. `row_security` is
-- disabled so the table owner bypasses sessions' FORCE RLS (otherwise the
-- unset app.user_id would silently match zero rows). No-op on a fresh DB.
-- Irreversible — the down migration cannot restore deleted rows.
SET LOCAL row_security = off;
DELETE FROM sessions;

-- ── RLS: org isolation (defense-in-depth). Member-scoping itself is enforced
-- in the thread-list query and the ChannelStore, not here. ──
ALTER TABLE channels ENABLE ROW LEVEL SECURITY;
ALTER TABLE channels FORCE ROW LEVEL SECURITY;
CREATE POLICY channels_org_isolation ON channels
    FOR ALL TO PUBLIC
    USING      (app_user_is_member(org_id))
    WITH CHECK (app_user_is_member(org_id));

ALTER TABLE channel_members ENABLE ROW LEVEL SECURITY;
ALTER TABLE channel_members FORCE ROW LEVEL SECURITY;
CREATE POLICY channel_members_org_isolation ON channel_members
    FOR ALL TO PUBLIC
    USING      (app_user_is_member(org_id))
    WITH CHECK (app_user_is_member(org_id));

-- ── Eager seeding for future members, mirroring colleagues' mint trigger
-- (migration 58). `#general` is created lazily on the FIRST membership rather
-- than on org insert: the channels RLS `WITH CHECK (app_user_is_member)` only
-- passes once a member exists, and an org is otherwise created before anyone
-- belongs to it (e.g. a tenant-scoped `INSERT INTO organizations`). The
-- membership trigger ensures `#general` then enrolls the new member — both
-- idempotent, timestamps flowing from the parent row (§11). ──
CREATE FUNCTION enroll_member_general_channel() RETURNS TRIGGER LANGUAGE plpgsql AS $$
DECLARE
    general_id UUID;
BEGIN
    INSERT INTO channels (id, org_id, name, created_by_user_id, created_at, archived_at)
    VALUES (gen_random_uuid(), NEW.org_id, 'general', NULL, NEW.created_at, NULL)
    ON CONFLICT (org_id, name) WHERE archived_at IS NULL DO NOTHING;

    SELECT id INTO general_id
      FROM channels
     WHERE org_id = NEW.org_id AND name = 'general' AND archived_at IS NULL;

    INSERT INTO channel_members (channel_id, user_id, org_id, added_at)
    VALUES (general_id, NEW.user_id, NEW.org_id, NEW.created_at)
    ON CONFLICT (channel_id, user_id) DO NOTHING;
    RETURN NEW;
END $$;

CREATE TRIGGER org_members_enroll_general
    AFTER INSERT ON org_members
    FOR EACH ROW EXECUTE FUNCTION enroll_member_general_channel();
