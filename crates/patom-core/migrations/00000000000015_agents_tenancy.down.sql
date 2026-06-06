-- Reverse migration 15.

DROP POLICY IF EXISTS agents_org_isolation ON agents;
ALTER TABLE agents DISABLE ROW LEVEL SECURITY;

DROP INDEX IF EXISTS agents_name_lower_unique;
CREATE UNIQUE INDEX agents_name_lower_unique
    ON agents ((lower(name)));

DROP INDEX IF EXISTS agents_default_unique;
CREATE UNIQUE INDEX agents_default_unique
    ON agents (is_default)
    WHERE is_default;

ALTER TABLE agents DROP COLUMN IF EXISTS org_id;
