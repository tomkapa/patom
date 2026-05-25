-- MCP catalog (R2).
--
-- Source of truth for "what MCP integrations does the system understand".
-- Built-in entries (org_id IS NULL) ship via the seed at the bottom of this
-- migration so every tenant sees the same baseline; tenant-custom entries
-- live in the same table with org_id set.
--
-- The recruiter agent consults this table (through the new `search_tools`
-- system tool) to suggest MCPs by stable id (e.g. "notion") instead of
-- raw `mcp_servers.id` UUIDs.
--
-- Visibility (RLS):
--   * SELECT — global rows readable by everyone; org-scoped rows readable
--     by that org's members only.
--   * INSERT/UPDATE/DELETE — only on rows the calling user's org owns.
--     Global rows are managed exclusively by migrations / privileged
--     roles (RLS off).
--
-- CLAUDE.md §14: paired down migration drops the table.

CREATE TABLE mcp_catalog (
    id                TEXT NOT NULL
                      CHECK (id ~ '^[a-z][a-z0-9_-]{0,39}$'),
    org_id            UUID REFERENCES organizations(id) ON DELETE CASCADE,
    display_name      TEXT NOT NULL CHECK (octet_length(display_name) BETWEEN 1 AND 100),
    description       TEXT NOT NULL CHECK (octet_length(description) BETWEEN 1 AND 1024),
    homepage_url      TEXT CHECK (homepage_url IS NULL OR octet_length(homepage_url) <= 2048),
    default_transport JSONB NOT NULL,
    auth_kind         TEXT NOT NULL CHECK (auth_kind IN ('oauth2','static_headers','none')),
    created_at        TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at        TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- Two partial unique indexes encode the collision rules:
--   * global ids (org_id IS NULL) are unique across the whole system —
--     no two built-ins may share an id.
--   * per-org ids are unique within their org — a tenant may shadow a
--     global id (intentionally; resolution prefers org-scoped) but may
--     not list the same custom id twice.
CREATE UNIQUE INDEX mcp_catalog_global_id_key
    ON mcp_catalog (id) WHERE org_id IS NULL;
CREATE UNIQUE INDEX mcp_catalog_org_id_key
    ON mcp_catalog (org_id, id) WHERE org_id IS NOT NULL;

-- Lookup index: tenants resolve catalog_id → row scoped to their org +
-- the global fallback in one indexed scan.
CREATE INDEX mcp_catalog_id_lookup_idx ON mcp_catalog (id);

ALTER TABLE mcp_catalog ENABLE ROW LEVEL SECURITY;
ALTER TABLE mcp_catalog FORCE ROW LEVEL SECURITY;

-- SELECT: global rows visible to all; org-scoped only to that org's members.
CREATE POLICY mcp_catalog_select ON mcp_catalog
    FOR SELECT TO PUBLIC
    USING (org_id IS NULL OR app_user_is_member(org_id));

-- INSERT: only into your own org (global rows are migration-only).
CREATE POLICY mcp_catalog_insert ON mcp_catalog
    FOR INSERT TO PUBLIC
    WITH CHECK (org_id IS NOT NULL AND app_user_is_member(org_id));

-- UPDATE: only on rows your org owns.
CREATE POLICY mcp_catalog_update ON mcp_catalog
    FOR UPDATE TO PUBLIC
    USING      (org_id IS NOT NULL AND app_user_is_member(org_id))
    WITH CHECK (org_id IS NOT NULL AND app_user_is_member(org_id));

-- DELETE: only on rows your org owns.
CREATE POLICY mcp_catalog_delete ON mcp_catalog
    FOR DELETE TO PUBLIC
    USING (org_id IS NOT NULL AND app_user_is_member(org_id));

-- ───────────────────────────────────────────────────────────────────────────
-- Seed built-in catalog entries (org_id NULL).
--
-- URLs reflect each vendor's canonical public MCP endpoint at the time of
-- writing; operators may override per-org via a custom catalog row that
-- shadows the global id, or amend in a follow-up migration if a vendor
-- moves their endpoint. `ON CONFLICT DO NOTHING` keeps the migration
-- idempotent against a fresh DB.
-- ───────────────────────────────────────────────────────────────────────────

INSERT INTO mcp_catalog (id, org_id, display_name, description, homepage_url, default_transport, auth_kind)
VALUES
    (
        'notion',
        NULL,
        'Notion',
        'Notion workspace integration. Use for free-form docs, wikis, meeting notes, and lightweight project pages. Exposes search across pages and the ability to read, create, and append page content.',
        'https://developers.notion.com/guides/mcp/overview',
        '{"type":"http","url":"https://mcp.notion.com/mcp"}'::jsonb,
        'oauth2'
    ),
    (
        'linear',
        NULL,
        'Linear',
        'Linear issue tracker integration. Use for structured product/engineering work: issues, projects, cycles, triage queues. Exposes issue search, create, comment, and status transitions.',
        'https://linear.app/docs/mcp',
        '{"type":"http","url":"https://mcp.linear.app/sse"}'::jsonb,
        'oauth2'
    ),
    (
        'slack',
        NULL,
        'Slack',
        'Slack workspace integration. Use for cross-team messaging context: channel history, threads, posting to channels or DMs. Pick for agents whose role is to monitor, summarise, or respond inside Slack.',
        'https://docs.slack.dev/ai/slack-mcp-server/',
        '{"type":"http","url":"https://mcp.slack.com/mcp"}'::jsonb,
        'oauth2'
    ),
    (
        'jira',
        NULL,
        'Jira',
        'Atlassian Jira integration. Use when the team tracks work in Jira rather than Linear: issue search, create, transition, comment. Includes Confluence-adjacent context where the same Atlassian MCP exposes it.',
        'https://support.atlassian.com/atlassian-rovo-mcp-server/docs/getting-started-with-the-atlassian-remote-mcp-server/',
        '{"type":"http","url":"https://mcp.atlassian.com/v1/sse"}'::jsonb,
        'oauth2'
    )
ON CONFLICT DO NOTHING;
