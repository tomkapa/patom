-- Slack adapter — Phase 2 (per-platform display name, GitHub issue #41).
--
-- A user's name is surface-specific: their Patom/IdP name, their Slack
-- handle, their Lark name. `colleague_id` stays the canonical key for
-- identity (memory, send_message, all logic); this column holds only the
-- *Slack* display label so the agent refers to people by the name their
-- teammates know in Slack when it renders a Slack-rooted thread. It never
-- changes `users.display_name` — Patom identity is not derived from Slack.
--
-- Captured from `users.info` at link time and refreshed opportunistically
-- when the bridge already has the profile. Nullable: an un-fetched or
-- failed lookup leaves it NULL and the renderer falls back to the
-- canonical colleague name.
ALTER TABLE slack_identities
    ADD COLUMN display_name TEXT
        CHECK (display_name IS NULL OR octet_length(display_name) BETWEEN 1 AND 200);
