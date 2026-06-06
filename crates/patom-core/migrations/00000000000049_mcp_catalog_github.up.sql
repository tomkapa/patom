-- GitHub-managed MCP catalog entry.
--
-- Adds one built-in (org_id IS NULL) catalog row for GitHub's official
-- remote MCP server announced in 2026:
--
--   * github — `https://api.githubcopilot.com/mcp/`
--              (https://docs.github.com/en/copilot/how-tos/provide-context/use-mcp/set-up-the-github-mcp-server)
--
-- The endpoint speaks the streamable HTTP MCP transport and authenticates
-- via OAuth 2.0 (github.com identity). GitHub's authorization server does
-- NOT support RFC 7591 Dynamic Client Registration; the per-tenant flow
-- resolves the OAuth client through the shared-client seeder
-- (`src/mcp/oauth/shared_seed.rs`) which upserts one row keyed by issuer
-- `https://github.com/login/oauth` — the exact value
-- api.githubcopilot.com's protected-resource metadata advertises — from
-- `PATOM_GITHUB_CLIENT_ID` / `_SECRET` at boot. Those env vars are
-- required at startup (see `AuthSettings`), so a Patom that booted at
-- all has a working `github` shared client.
--
-- Stable id matches the convention established by migrations 30 (notion,
-- linear, slack, jira) and 36 (gmail, gcal) so agent role prompts can
-- alias against `github` without scaffolding.
--
-- `default_scope` (column added by migration 38) carries the four scopes
-- GitHub's MCP server checks against to enable the full default toolset:
--   * `repo`           — repository read/write (issues, PRs, code)
--   * `read:packages`  — Docker image / package metadata access
--   * `read:org`       — organization team membership (for tool filtering)
--   * `read:user`      — authenticated-user identity (for display name)
-- The server itself filters tools by token scope at runtime, so requesting
-- a superset that the OAuth App is not approved for is harmless — the
-- consent screen reduces to the App's actual scope set.
--
-- `authorize_extra_params` (column added by migration 39) is left NULL —
-- GitHub's authorize endpoint is vanilla RFC 6749 §4.1 with no Google-
-- shaped `access_type=offline` / `prompt=consent` quirk needed. If the
-- OAuth App is registered with "Expire user authorization tokens" ON,
-- refresh tokens flow without extra params; if OFF, tokens are
-- non-expiring and the refresher is a no-op for these rows.
--
-- Icon URL follows the asset.tomkapa.uk pattern established by migration
-- 33; the SVG (`mcp/github.svg`) must be uploaded to the R2 bucket
-- out-of-band BEFORE this migration runs in any environment. Without it
-- the FE renders a 404 for the tile until the SVG lands — the catalog row
-- is still queryable, only the `<img>` request 404s, so the Monogram
-- fallback keeps the page usable.
--
-- CLAUDE.md §14: paired down migration deletes the row.

INSERT INTO mcp_catalog (
    id,
    org_id,
    display_name,
    description,
    homepage_url,
    default_transport,
    auth_kind,
    icon_url,
    default_scope
)
VALUES (
    'github',
    NULL,
    'GitHub',
    'GitHub integration via the official remote MCP server. Use for agents that read repositories, search and triage issues and pull requests, review code, or manage GitHub Projects on behalf of a member. Exposes repo read/write, issue/PR search and comment, code search, and Projects tools — filtered server-side to the token''s actual OAuth scopes.',
    'https://docs.github.com/en/copilot/how-tos/provide-context/use-mcp/set-up-the-github-mcp-server',
    '{"type":"http","url":"https://api.githubcopilot.com/mcp/"}'::jsonb,
    'oauth2',
    'https://asset.tomkapa.uk/mcp/github.svg',
    'repo read:packages read:org read:user'
)
ON CONFLICT DO NOTHING;
