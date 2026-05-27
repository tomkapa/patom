-- The `history_count` column was part of migration 44's first cut but was
-- removed from the row, the INSERT, and the HTTP response over subsequent
-- iterations. Dev databases that applied the first version of 44 still
-- carry the column NOT NULL, which would block the (now-wired) recorder
-- INSERT. Pre-launch: `IF EXISTS` so fresh DBs (where 44 already lacks
-- the column) are no-ops, and live dev DBs drop it on first apply.
ALTER TABLE turn_metrics DROP COLUMN IF EXISTS history_count;
