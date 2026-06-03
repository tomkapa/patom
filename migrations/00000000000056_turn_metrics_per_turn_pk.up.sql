-- Re-key turn_metrics: one row PER PROVIDER CALL, not per request.
--
-- The table was keyed `request_id PRIMARY KEY`, but `request_id` is constant
-- across every turn of a reply (and across retries via `resume`). So the
-- second turn of any tool-using reply collided on `turn_metrics_pkey`
-- (Postgres 23505) — the recorder swallows the error (best-effort, CLAUDE.md
-- §6), silently dropping every turn after the first and undercounting cost.
--
-- Switch to a synthetic per-row id. `request_id` keeps its FK to
-- prompt_requests (column-level REFERENCES, untouched by the PK swap) and
-- stays NOT NULL; a non-unique index serves per-request lookups.
--
-- Pre-launch single-step migration: dev DBs are wiped before applying
-- (feedback_no_backcompat), so no live rows need a real id backfill — the
-- transient DEFAULT only keeps the ADD COLUMN valid on a populated table.

ALTER TABLE turn_metrics
    ADD COLUMN id UUID NOT NULL DEFAULT gen_random_uuid();
ALTER TABLE turn_metrics
    ALTER COLUMN id DROP DEFAULT;

ALTER TABLE turn_metrics
    DROP CONSTRAINT turn_metrics_pkey;
ALTER TABLE turn_metrics
    ADD CONSTRAINT turn_metrics_pkey PRIMARY KEY (id);

-- `request_id` stays NOT NULL: Postgres retains the column's NOT NULL when its
-- PRIMARY KEY is dropped, so no re-assertion is needed here.

-- Per-request lookups (turn-detail drawer, drilldowns) — non-unique now.
CREATE INDEX turn_metrics_request_idx
    ON turn_metrics (request_id);
