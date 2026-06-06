-- Google-managed MCP catalog entries (Gmail + Calendar).
--
-- Adds two built-in (org_id IS NULL) catalog rows for the Google-managed
-- remote MCP servers announced in 2026:
--
--   * gmail — `https://gmailmcp.googleapis.com/mcp/v1`
--             (https://developers.google.com/workspace/gmail/api/guides/configure-mcp-server)
--   * gcal  — `https://calendarmcp.googleapis.com/mcp/v1`
--             (https://developers.google.com/workspace/calendar/api/guides/configure-mcp-server)
--
-- Both endpoints speak the streamable HTTP MCP transport and authenticate
-- via OAuth 2.0 (Google identity). The per-org client_id / client_secret
-- and granted scopes are configured at `mcp_servers` row creation time;
-- the catalog only records the endpoint + auth_kind so the recruiter
-- agent can surface them via `search_tools` by stable id.
--
-- Stable ids match the role-prompt aliases used by `doc/pitch_demo.md`
-- (`gmail`, `gcal`) so existing scenario bodies resolve cleanly.
--
-- Icon URLs follow the asset.tomkapa.uk pattern established by migration
-- 33; the two SVGs (`mcp/gmail.svg`, `mcp/gcal.svg`) must be uploaded to
-- the R2 bucket out-of-band BEFORE this migration runs in any
-- environment. Without them the FE renders 404s for these two tiles
-- until the SVGs land — the catalog row is still queryable, only the
-- `<img>` request 404s, so the Monogram fallback keeps the page usable.
--
-- CLAUDE.md §14: paired down migration deletes the two rows.

INSERT INTO mcp_catalog (id, org_id, display_name, description, homepage_url, default_transport, auth_kind, icon_url)
VALUES
    (
        'gmail',
        NULL,
        'Gmail',
        'Gmail mailbox integration via the Google-managed remote MCP server. Use for agents that read, search, draft, or send email on behalf of a Workspace user — customer correspondence, status updates, follow-up nudges. Exposes message search, fetch, compose, and send.',
        'https://developers.google.com/workspace/gmail/api/guides/configure-mcp-server',
        '{"type":"http","url":"https://gmailmcp.googleapis.com/mcp/v1"}'::jsonb,
        'oauth2',
        'https://asset.tomkapa.uk/mcp/gmail.svg'
    ),
    (
        'gcal',
        NULL,
        'Google Calendar',
        'Google Calendar integration via the Google-managed remote MCP server. Use for agents that schedule meetings, check free/busy, or place deadlines on a Workspace calendar. Exposes calendar list, event search, event create, and free/busy lookup.',
        'https://developers.google.com/workspace/calendar/api/guides/configure-mcp-server',
        '{"type":"http","url":"https://calendarmcp.googleapis.com/mcp/v1"}'::jsonb,
        'oauth2',
        'https://asset.tomkapa.uk/mcp/gcal.svg'
    )
ON CONFLICT DO NOTHING;
