-- Backfill the issue #121 default monthly budget cap onto orgs that never had
-- an `org_budgets` row.
--
-- Before this change a new org was created uncapped (no row), which the budget
-- gate (src/budget/service.rs) treats as *unlimited* — so every pre-existing
-- beta org could run uncapped spend on our provider bill. Migration 57 closes
-- that hole for the existing fleet; new orgs are stamped at creation time
-- (src/auth/pg_store.rs).
--
-- Only orgs with NO row are touched. An org with an existing row — a configured
-- cap OR an admin-chosen NULL (explicit unlimited) — is left as-is: respecting a
-- deliberate choice, not overwriting it.
--
-- Pure data backfill: no schema change, no column default, no table rewrite. It
-- reads `organizations` and inserts into `org_budgets`; both are small at beta
-- scale and the INSERT takes only ordinary row locks on the new rows. Runs as
-- the privileged migration role, which bypasses the migration-54 RLS policy.
--
-- Values mirror DEFAULT_ORG_MONTHLY_CAP_MICROS (1_000_000 = $1.00) and
-- DEFAULT_WARN_BPS (8000 = 80%) in src/budget/limits.rs — keep the three in sync.
-- Idempotent: re-running inserts zero rows (the LEFT JOIN ... IS NULL guard).
INSERT INTO org_budgets (org_id, monthly_cap_micro_usd, warn_threshold_bps, created_at, updated_at)
SELECT o.id, 1000000, 8000, now(), now()
FROM organizations o
LEFT JOIN org_budgets b ON b.org_id = o.id
WHERE b.org_id IS NULL;
