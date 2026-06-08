-- Reverse of 00000000000062_channels.up.sql.
--
-- Lossy: the thread rows the up migration deleted as part of the full reset
-- cannot be restored. This only tears down the channel surface.

DROP TRIGGER IF EXISTS org_members_enroll_general ON org_members;
DROP FUNCTION IF EXISTS enroll_member_general_channel();

-- Drops the FK to channels along with the column.
ALTER TABLE prompt_requests DROP COLUMN channel_id;

DROP TABLE channel_members;
DROP TABLE channels;
