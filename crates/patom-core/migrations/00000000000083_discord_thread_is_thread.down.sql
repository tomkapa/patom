-- Reverse 00000000000083_discord_thread_is_thread.up.sql.
--
-- `is_thread` was backfilled from `parent_id IS NOT NULL`; dropping the column
-- returns to deriving it at read time, so no data is lost for the
-- bot-opened-thread case (only the learned "non-threadable container" bit, which
-- is re-learned on the next mention).
ALTER TABLE discord_threads DROP COLUMN is_thread;
