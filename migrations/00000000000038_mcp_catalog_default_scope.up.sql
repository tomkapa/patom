-- Default OAuth scope per catalog entry.
--
-- Some integrations require an explicit `scope` parameter on the
-- authorize request (Google rejects with `Missing required parameter:
-- scope` otherwise). Per-integration, not per-issuer: Gmail and
-- Calendar share the same issuer (`accounts.google.com`) but a tenant
-- connecting Gmail must only see Gmail scopes on the consent screen,
-- not the union with Calendar.
--
-- Storage shape: a single space-separated scope list (RFC 6749 §3.3
-- wire format). The OAuth flow forwards it verbatim into the authorize
-- URL's `scope` param when the request body omits its own override.
--
-- DCR vendors (Notion, Linear, Slack, Jira) leave this NULL — the AS
-- applies its own default scope set during registration, no client-side
-- declaration needed.
--
-- 2048-byte cap mirrors `homepage_url` so list responses stay bounded.
--
-- CLAUDE.md §14: paired down drops the column.

ALTER TABLE mcp_catalog
    ADD COLUMN default_scope TEXT
    CHECK (default_scope IS NULL OR octet_length(default_scope) BETWEEN 1 AND 2048);

-- Gmail: read inbox + draft/send. Restricted scopes — work in
-- Testing-mode consent screens; production needs Google's CASA review.
UPDATE mcp_catalog
   SET default_scope = 'https://www.googleapis.com/auth/gmail.readonly '
                       || 'https://www.googleapis.com/auth/gmail.compose',
       updated_at = now()
 WHERE id = 'gmail' AND org_id IS NULL;

-- Calendar: create/edit events + free/busy queries + list calendars.
-- All non-sensitive as of 2026 — no verification required.
UPDATE mcp_catalog
   SET default_scope = 'https://www.googleapis.com/auth/calendar.events '
                       || 'https://www.googleapis.com/auth/calendar.events.freebusy '
                       || 'https://www.googleapis.com/auth/calendar.readonly',
       updated_at = now()
 WHERE id = 'gcal' AND org_id IS NULL;
