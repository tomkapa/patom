-- org_egress_allowlist — per-org outbound network allowlist for run_code (#218).
--
-- Default-deny: an org with no row (or an empty array) runs the sandbox with
-- --network=none. Each entry is a bare host validated host-side through the same
-- SSRF deny floor web_fetch uses (the EgressHost newtype), so localhost,
-- RFC1918, link-local, and the cloud metadata endpoint can never be stored.
-- One row per org; the app-side EgressAllowlist caps the count and re-validates
-- every entry on read (fail-closed against a tampered row).
CREATE TABLE org_egress_allowlist (
    org_id     UUID        NOT NULL PRIMARY KEY REFERENCES organizations(id) ON DELETE CASCADE,
    -- JSONB array of bare host strings. The CHECK keeps the shape an array; the
    -- per-entry validation (deny floor + count cap) is enforced in the app on
    -- every read and write, since SQL can't express the SSRF rules.
    hosts      JSONB       NOT NULL DEFAULT '[]'::jsonb CHECK (jsonb_typeof(hosts) = 'array'),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- Org isolation, mirroring tool_artifacts / the rest of the schema. The store's
-- read path also binds org explicitly (memory: rls-gates-membership-not-active-org).
ALTER TABLE org_egress_allowlist ENABLE ROW LEVEL SECURITY;
ALTER TABLE org_egress_allowlist FORCE ROW LEVEL SECURITY;
CREATE POLICY org_egress_allowlist_org_isolation ON org_egress_allowlist
    FOR ALL TO PUBLIC
    USING      (app_user_is_member(org_id))
    WITH CHECK (app_user_is_member(org_id));
