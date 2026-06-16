-- Reverse of 00000000000079_colleague_profiles.up.sql.

-- Undo the agents-side index + column resize. Returning the column to the
-- original UNSIZED `vector` keeps the down a faithful inverse of the up.
DROP INDEX IF EXISTS agents_description_embedding_hnsw;
ALTER TABLE agents
    ALTER COLUMN description_embedding TYPE vector
        USING description_embedding::vector;

-- Drop the profile board (indexes + policy fall with the table, but the policy
-- is dropped explicitly to mirror the colleagues down migration).
DROP POLICY IF EXISTS colleague_profiles_org_isolation ON colleague_profiles;
DROP TABLE IF EXISTS colleague_profiles;
