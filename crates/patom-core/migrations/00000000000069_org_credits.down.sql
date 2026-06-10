-- Reverse the credit ledger migration (#154). Dropping the tables removes their
-- RLS policies, indexes, and the REVOKE with them.
DROP TABLE IF EXISTS org_credit_ledger;
DROP TABLE IF EXISTS org_credits;
