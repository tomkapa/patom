-- Revert turn_metrics to a `request_id`-keyed table.
--
-- LOSSY BY CONSTRUCTION: restoring the PRIMARY KEY on `request_id` fails if the
-- forward schema accumulated more than one row per request (the very rows the
-- forward migration exists to allow). Safe only against a fresh / single-turn
-- dataset, which is the pre-launch case this pair targets.

DROP INDEX IF EXISTS turn_metrics_request_idx;

ALTER TABLE turn_metrics
    DROP CONSTRAINT turn_metrics_pkey;
ALTER TABLE turn_metrics
    DROP COLUMN id;

ALTER TABLE turn_metrics
    ADD CONSTRAINT turn_metrics_pkey PRIMARY KEY (request_id);
