-- Reverse the issue #121 default-cap backfill.
--
-- The backfill mixed default-shaped rows into the same table that holds
-- user-configured budgets, so a perfectly lossless rollback is impossible — we
-- cannot distinguish a backfilled row from one a user later set to the same
-- values. The pragmatic, safe choice: delete only rows STILL at the untouched
-- default shape ($1.00 cap, 80% warn). Any org an admin has since re-configured
-- (different cap or threshold) is preserved.
--
-- Cost: an org that *intentionally* set exactly $1.00 / 80% would lose its row
-- (reverting to unlimited) on rollback. That is rare, and a down-migration is an
-- emergency operation — acceptable. Keep the literals in sync with the up
-- migration and src/budget/limits.rs.
DELETE FROM org_budgets
WHERE monthly_cap_micro_usd = 1000000
  AND warn_threshold_bps = 8000;
