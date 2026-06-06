-- Per-catalog extra params appended to the OAuth authorize URL.
--
-- Some vendors require non-standard query parameters on the authorize
-- redirect that aren't part of RFC 6749 §4.1. Google needs two:
--   * `access_type=offline` — otherwise Google issues NO refresh_token
--     and the connection silently dies one hour after consent.
--   * `prompt=consent` — when a user previously granted a subset of
--     scopes for the same client, Google's server-side grant cache
--     returns the prior token unchanged on the next authorize even if
--     the URL requests additional scopes. `prompt=consent` forces
--     re-consent and re-issues a token covering the full scope set.
--
-- Promoted to data (catalog column) instead of code (`is_google_issuer`
-- match in `build_authorize_url`) so future vendors with their own
-- authorize-side quirks (Microsoft 365 `prompt=consent`+`prompt=login`,
-- Atlassian `audience=api.atlassian.com`, …) are a migration not a
-- recompile.
--
-- Storage shape: JSONB array of {key, value} objects in iteration order
-- — `[{"key":"access_type","value":"offline"}, ...]`. Objects (not a
-- map) so duplicates are representable (e.g. Microsoft's `prompt=login
-- prompt=consent` if a vendor ever needs key repetition).
--
-- DCR vendors (Notion, Linear, Slack, Jira) leave this NULL — the AS
-- doesn't expect non-standard params and may reject the redirect.
--
-- 2048-byte cap mirrors the other catalog text columns so list
-- responses stay bounded.
--
-- CLAUDE.md §14: paired down drops the column.

ALTER TABLE mcp_catalog
    ADD COLUMN authorize_extra_params JSONB
    CHECK (
        authorize_extra_params IS NULL
        OR (
            jsonb_typeof(authorize_extra_params) = 'array'
            AND octet_length(authorize_extra_params::text) BETWEEN 2 AND 2048
        )
    );

-- Gmail + Calendar share `accounts.google.com` and both need the same
-- two params (the quirks are issuer-scoped on Google's side, not
-- per-product).
UPDATE mcp_catalog
   SET authorize_extra_params = '[
           {"key": "access_type", "value": "offline"},
           {"key": "prompt",      "value": "consent"}
       ]'::jsonb,
       updated_at = now()
 WHERE id IN ('gmail', 'gcal') AND org_id IS NULL;
