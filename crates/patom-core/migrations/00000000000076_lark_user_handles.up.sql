-- Lark adapter — the persistent global people directory (design doc §4).
--
-- One stable Patom identity per Lark user per tenant — the same across every
-- thread and channel. Every observed sender (and every roster member, incl.
-- silent ones who never post) materializes here, backed by a shadow colleague
-- until/unless the person becomes a real Patom user (the shadow→real merge is
-- deferred, §9b).
--
-- PK on (tenant_key, lark_user_id): the identity is scoped to the TENANT, not
-- the app, so two bots in one tenant share one shadow per person. lark_user_id
-- is the stable key (== Lark "employee_id"; contact:user.employee_id:readonly
-- is a hard setup gate so in-tenant members always carry it). open_id is the
-- per-bot satellite — the @-tag handle (and, for the deferred backfill, the
-- history join key). colleague_id is the durable join target; the FK is on
-- colleague_id alone (colleagues has no org-composite unique to target), matching
-- threads.dm_counterpart_colleague_id (migration 66) — the colleague's own
-- org_id is enforced by the mint trigger writing NEW.org_id.

CREATE TABLE lark_user_handles (
    org_id        UUID NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    tenant_key    TEXT NOT NULL CHECK (octet_length(tenant_key) BETWEEN 1 AND 128),
    lark_user_id  TEXT NOT NULL CHECK (octet_length(lark_user_id) BETWEEN 1 AND 128),
    open_id       TEXT NOT NULL CHECK (octet_length(open_id) BETWEEN 1 AND 128),
    colleague_id  UUID NOT NULL REFERENCES colleagues(id) ON DELETE CASCADE,
    created_at    TIMESTAMPTZ NOT NULL,
    PRIMARY KEY (tenant_key, lark_user_id)
);

-- The poster's outbound @-tag resolve is colleague_id -> open_id; the reverse
-- lookup needs an index so it is a point read.
CREATE INDEX lark_user_handles_colleague_idx ON lark_user_handles (org_id, colleague_id);

ALTER TABLE lark_user_handles ENABLE ROW LEVEL SECURITY;
ALTER TABLE lark_user_handles FORCE ROW LEVEL SECURITY;
CREATE POLICY lark_user_handles_org_isolation ON lark_user_handles
    FOR ALL TO PUBLIC
    USING      (app_user_is_member(org_id))
    WITH CHECK (app_user_is_member(org_id));
