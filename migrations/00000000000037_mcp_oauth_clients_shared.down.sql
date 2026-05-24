-- Reverse the shared-OAuth-clients schema change.
--
-- Order matters: any shared (org_id IS NULL) rows must be removed before
-- we can restore the NOT NULL + composite PK. The seeder is the only
-- writer of those rows, so this delete is safe between deploys (boot
-- will re-seed on the way back forward).

DELETE FROM mcp_oauth_clients WHERE org_id IS NULL;

DROP POLICY mcp_oauth_clients_select ON mcp_oauth_clients;
DROP POLICY mcp_oauth_clients_insert ON mcp_oauth_clients;
DROP POLICY mcp_oauth_clients_update ON mcp_oauth_clients;
DROP POLICY mcp_oauth_clients_delete ON mcp_oauth_clients;

CREATE POLICY mcp_oauth_clients_org_isolation ON mcp_oauth_clients
    FOR ALL TO PUBLIC
    USING      (app_user_is_member(org_id))
    WITH CHECK (app_user_is_member(org_id));

DROP INDEX mcp_oauth_clients_shared_issuer_key;
DROP INDEX mcp_oauth_clients_org_issuer_key;

ALTER TABLE mcp_oauth_clients ALTER COLUMN org_id SET NOT NULL;

ALTER TABLE mcp_oauth_clients
    ADD CONSTRAINT mcp_oauth_clients_pkey PRIMARY KEY (org_id, issuer);
