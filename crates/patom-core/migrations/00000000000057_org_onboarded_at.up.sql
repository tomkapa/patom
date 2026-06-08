-- Onboarding-completion flag.
--
-- `onboarded_at IS NULL` means the user has signed in but not yet walked
-- the /onboarding wizard (name workspace → pick a team → invite). The FE
-- gate redirects such users into the wizard; the wizard's final step
-- (Send invites / Skip for now) sends PATCH /me/org { onboarded: true },
-- which calls `mark_onboarded` and stamps NOW() here.
--
-- Backfill: every row that already exists predates this column and so
-- belongs to a user who walked the legacy "auto-create + drop into
-- workspace" path. Stamping them with NOW() means existing prod users
-- aren't shoved into the wizard on their next page load (CLAUDE.md §14
-- — never strand the live tenant).
--
-- Nullable on purpose: we want to distinguish "never onboarded" from
-- "onboarded at a specific point in time" so future analytics can read
-- the timestamp. The FE only cares about NULL vs. not-NULL, exposed as
-- `onboarded: bool` on `OrgView`.

ALTER TABLE organizations
    ADD COLUMN onboarded_at TIMESTAMPTZ;

UPDATE organizations SET onboarded_at = NOW() WHERE onboarded_at IS NULL;
