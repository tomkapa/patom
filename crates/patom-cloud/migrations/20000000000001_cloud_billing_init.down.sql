-- Reverse of the cloud billing init. Policies and indexes drop with their
-- tables. The `cloud` schema itself is left in place: it is created
-- idempotently by patom-cloud's Migrator (not by a migration), and it also
-- owns the `cloud._sqlx_migrations` tracking table this rollback is recorded
-- in, so dropping it here would be self-defeating.
DROP TABLE IF EXISTS cloud.webhook_events;
DROP TABLE IF EXISTS cloud.subscriptions;
