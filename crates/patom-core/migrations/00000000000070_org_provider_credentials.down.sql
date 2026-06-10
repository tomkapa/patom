-- Reverse the BYO provider-credentials table (#141). Dropping the table removes
-- its RLS policy with it.
DROP TABLE IF EXISTS org_provider_credentials;
