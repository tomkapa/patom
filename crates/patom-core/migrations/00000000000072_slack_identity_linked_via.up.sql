-- Slack adapter — Phase 2 (per-user identity linking, GitHub issue #41).
--
-- Records the provenance of each `slack_identities` row so the two link
-- sources stay auditable and policy can change later without a rewrite:
--   - 'installer'   — written automatically at OAuth install from the
--                     Slack `authed_user.id`; the workspace owner is
--                     linked before they ever run `/patom`.
--   - 'slack_oauth' — written by the post-login completion route after an
--                     unlinked user authenticates to Patom via `/patom`.
--
-- Nullable with no backfill: Phase 1 never populated `slack_identities`,
-- so the table is empty in every environment and the paired `.down.sql`
-- is a clean `DROP COLUMN`.
ALTER TABLE slack_identities
    ADD COLUMN linked_via TEXT
        CHECK (linked_via IS NULL OR linked_via IN ('installer', 'slack_oauth'));
