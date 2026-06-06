-- Revert turn_metrics to a `request_id`-keyed table.
--
-- LOSSY BY CONSTRUCTION: restoring the PRIMARY KEY on `request_id` FAILS once
-- the forward schema accumulated more than one row per request — the very rows
-- the forward migration exists to allow. In production (multi-turn replies
-- exist), this rollback will error on the duplicate `request_id`s, so treat
-- the forward migration as effectively one-way: a genuine rollback must first
-- collapse/dedup turn_metrics to one row per request_id (losing per-turn data).
-- Prefer rolling forward with a fix over reverting.

DROP INDEX IF EXISTS turn_metrics_request_idx;

ALTER TABLE turn_metrics
    DROP CONSTRAINT turn_metrics_pkey;
ALTER TABLE turn_metrics
    DROP COLUMN id;

ALTER TABLE turn_metrics
    ADD CONSTRAINT turn_metrics_pkey PRIMARY KEY (request_id);
