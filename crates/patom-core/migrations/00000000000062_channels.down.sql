-- Reverse of 00000000000062_channels.up.sql. Tears down the channel surface;
-- `prompt_requests.channel_id` values are dropped with the column.

DROP TRIGGER IF EXISTS org_members_enroll_general ON org_members;
DROP FUNCTION IF EXISTS enroll_member_general_channel();

-- Drops the FK to channels along with the column.
ALTER TABLE prompt_requests DROP COLUMN channel_id;

DROP TABLE channel_members;
DROP TABLE channels;
