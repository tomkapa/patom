-- Cluster-global prerequisite for the tenancy migration
-- (migrations/00000000000014_tenancy_foundation.up.sql).
--
-- Postgres roles are scoped to the *cluster*, not to a database. That migration
-- guards creation with `IF NOT EXISTS ... CREATE ROLE patom_app`, which is fine
-- single-threaded but races (TOCTOU) when `#[sqlx::test]` applies migrations
-- across many fresh databases in the same cluster concurrently: two test
-- databases both observe "role absent" and both `CREATE ROLE`, so one fails with
-- a duplicate-key error on pg_authid.
--
-- Creating the role once per cluster *before* any migration runs makes that
-- guard always take the skip branch, eliminating the race. The migration still
-- owns the role's GRANTs (those are per-database and don't race). Idempotent —
-- safe to run repeatedly and on a cluster where the role already exists.
DO $$
BEGIN
    IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'patom_app') THEN
        CREATE ROLE patom_app NOLOGIN;
    END IF;
END
$$;
