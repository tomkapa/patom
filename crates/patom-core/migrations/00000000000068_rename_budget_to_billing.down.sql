-- Reverse the budget → billing rename (issue #154). Mirror image of the up
-- migration: rename policies and PK constraints back, then the tables.

ALTER POLICY org_billing_org_isolation       ON org_billing       RENAME TO org_budgets_org_isolation;
ALTER POLICY org_billing_usage_org_isolation ON org_billing_usage RENAME TO org_budget_usage_org_isolation;

ALTER TABLE org_billing       RENAME CONSTRAINT org_billing_pkey       TO org_budgets_pkey;
ALTER TABLE org_billing_usage RENAME CONSTRAINT org_billing_usage_pkey TO org_budget_usage_pkey;

ALTER TABLE org_billing       RENAME TO org_budgets;
ALTER TABLE org_billing_usage RENAME TO org_budget_usage;
