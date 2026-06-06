-- Agents tenancy retrofit. Follows the `mcp_servers` pattern landed in
-- migration 14: add `org_id`, swap the global uniqueness indexes for
-- per-org variants, enable + force RLS, and install an isolation policy
-- that mirrors `mcp_servers_org_isolation`.
--
-- The "exactly one default agent" rule (originally a global partial
-- unique index) becomes "exactly one default per org" so each tenant
-- workspace owns its own default agent. Name uniqueness on `lower(name)`
-- becomes per-org for the same reason — two orgs may both want a
-- `translator`.
--
-- Pre-launch: NOT NULL with no backfill (see `feedback_no_backcompat`).
-- Existing dev rows must be wiped before applying; the test schema is
-- minted fresh per test so the test path is naturally unaffected.

ALTER TABLE agents
    ADD COLUMN org_id UUID NOT NULL REFERENCES organizations(id) ON DELETE CASCADE;

-- Swap the global "one default per registry" for "one default per org".
DROP INDEX IF EXISTS agents_default_unique;
CREATE UNIQUE INDEX agents_default_unique
    ON agents (org_id)
    WHERE is_default;

-- Same for the case-insensitive name uniqueness — two `translator`s in
-- different orgs is fine; two in one org is operator error.
DROP INDEX IF EXISTS agents_name_lower_unique;
CREATE UNIQUE INDEX agents_name_lower_unique
    ON agents (org_id, (lower(name)));

ALTER TABLE agents ENABLE ROW LEVEL SECURITY;
ALTER TABLE agents FORCE ROW LEVEL SECURITY;
CREATE POLICY agents_org_isolation ON agents
    FOR ALL TO PUBLIC
    USING      (app_user_is_member(org_id))
    WITH CHECK (app_user_is_member(org_id));
