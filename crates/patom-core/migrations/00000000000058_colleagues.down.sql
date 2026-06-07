-- Reverse of 00000000000057_colleagues.up.sql.
DROP TRIGGER IF EXISTS org_members_mint_colleague ON org_members;
DROP TRIGGER IF EXISTS agents_mint_colleague ON agents;
DROP FUNCTION IF EXISTS mint_colleague();
DROP POLICY IF EXISTS colleagues_org_isolation ON colleagues;
DROP TABLE IF EXISTS colleagues;
