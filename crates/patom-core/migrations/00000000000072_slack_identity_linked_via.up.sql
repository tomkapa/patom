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
-- NOT NULL, no default: Phase 1 never populated `slack_identities`, so the
-- table is empty in every environment (an `ADD COLUMN NOT NULL` with no
-- default is safe) and every write path supplies `linked_via` — so the
-- audit invariant "every link has a known source" holds by construction.
-- The paired `.down.sql` is a clean `DROP COLUMN`.
ALTER TABLE slack_identities
    ADD COLUMN linked_via TEXT NOT NULL
        CHECK (linked_via IN ('installer', 'slack_oauth'));
