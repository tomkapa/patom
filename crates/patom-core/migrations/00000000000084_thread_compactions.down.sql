-- Revert the metering kind first. Fails if any `turn_metrics.kind='compaction'`
-- rows exist — drop them before rolling back (rollback discipline, CLAUDE.md §13).
ALTER TABLE turn_metrics DROP CONSTRAINT turn_metrics_kind_check;
ALTER TABLE turn_metrics
    ADD CONSTRAINT turn_metrics_kind_check
    CHECK (kind IN ('normal', 'reflection', 'resolution'));

DROP POLICY thread_compactions_org_isolation ON thread_compactions;
DROP TABLE thread_compactions;
