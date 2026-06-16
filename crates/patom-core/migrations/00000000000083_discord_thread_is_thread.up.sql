-- Discord adapter — record explicitly whether a bound container is a thread.
--
-- Previously `is_thread` was DERIVED from `parent_id IS NOT NULL`, which works
-- for a thread the bot opens (it knows the parent channel) but NOT for a
-- user-made thread the bot is @mentioned in: opening a thread-from-message there
-- fails (Discord 50024 "Cannot execute action on this channel type"), and the
-- parent is unknown — yet the bridge must still record that the container is a
-- thread so it converses there and never re-attempts (and re-fails) the open.
--
-- So `is_thread` becomes a first-class column: TRUE for a thread we open, for a
-- container a thread-open permanently rejected (already a thread / a forum), and
-- for a continuation in either; FALSE for a top-level channel or a DM.
ALTER TABLE discord_threads
    ADD COLUMN is_thread BOOLEAN NOT NULL DEFAULT FALSE;

-- Backfill from the prior derivation so existing bindings keep their meaning.
UPDATE discord_threads SET is_thread = TRUE WHERE parent_id IS NOT NULL;
