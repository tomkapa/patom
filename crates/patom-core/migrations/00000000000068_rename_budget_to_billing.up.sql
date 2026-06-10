-- Rename the spend-cap tables budget → billing (issue #154).
--
-- The billing module now owns credits as well as the monthly spend cap, so the
-- cap tables move under the `billing` name to match `src/billing/`. This is a
-- pure rename: rows, RLS, and constraints are preserved (ALTER ... RENAME), no
-- data movement. Auto-named CHECK constraints keep their original names (cosmetic
-- only, invisible at the app layer); the PK constraints and RLS policies are
-- renamed so the schema reads consistently.

ALTER TABLE org_budgets      RENAME TO org_billing;
ALTER TABLE org_budget_usage RENAME TO org_billing_usage;

ALTER TABLE org_billing       RENAME CONSTRAINT org_budgets_pkey      TO org_billing_pkey;
ALTER TABLE org_billing_usage RENAME CONSTRAINT org_budget_usage_pkey TO org_billing_usage_pkey;

ALTER POLICY org_budgets_org_isolation      ON org_billing       RENAME TO org_billing_org_isolation;
ALTER POLICY org_budget_usage_org_isolation ON org_billing_usage RENAME TO org_billing_usage_org_isolation;
