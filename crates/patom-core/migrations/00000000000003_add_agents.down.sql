-- Reverse 00000000000003_add_agents.up.sql.
ALTER TABLE sessions DROP COLUMN agent_id;
DROP INDEX IF EXISTS agents_default_unique;
DROP TABLE IF EXISTS agents;
