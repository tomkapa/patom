-- Restore the column so a rollback against a DB that originally had it
-- continues to satisfy the application's prior expectations. NOT NULL
-- with a default of 0 covers any rows inserted while the column was
-- absent.
ALTER TABLE turn_metrics
    ADD COLUMN IF NOT EXISTS history_count INTEGER NOT NULL DEFAULT 0
        CHECK (history_count >= 0);
