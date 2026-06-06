-- Scheduled tasks. An agent calls `schedule_task` to register a future
-- wake-up; the `ScheduledTaskScheduler` polls this table on a fixed cadence,
-- enqueues a `prompt_requests` row when `next_run_at <= now()`, and advances
-- the cursor.
--
-- The `schedule` column is a tagged-union JSONB:
--   {"kind":"once",      "data":{"run_at":"<iso8601-utc>"}}
--   {"kind":"recurring", "data":{"weekdays":["mon","tue",…],
--                                "time":"05:00", "tz":"<iana>"}}
-- JSONB so adding a future variant is a serde change, not a migration.
--
-- `next_run_at` is the materialised cursor — NULL means no further fires
-- (one-shot completed, or row cancelled). The partial index narrows the
-- scheduler's claim query to just due-and-active rows.
--
-- The scheduler's `claim_due` query is a plain SELECT; concurrent
-- scheduler nodes (future) dedupe at the queue layer via the
-- `sched-{task_id}-{fire_ts}` idempotency key.

CREATE TABLE scheduled_tasks (
    id              UUID PRIMARY KEY,
    owner_agent_id  UUID NOT NULL REFERENCES agents(id) ON DELETE CASCADE,

    name            TEXT NOT NULL
                    CHECK (octet_length(name) BETWEEN 1 AND 200),
    prompt          TEXT NOT NULL
                    CHECK (octet_length(prompt) BETWEEN 1 AND 65536),

    schedule        JSONB NOT NULL,

    next_run_at     TIMESTAMPTZ,
    last_fired_at   TIMESTAMPTZ,
    last_request_id UUID,
    state           TEXT NOT NULL
                    CHECK (state IN ('active','done','cancelled')),

    created_at      TIMESTAMPTZ NOT NULL,
    updated_at      TIMESTAMPTZ NOT NULL
);

-- Hot path for the scheduler: only active, due rows.
CREATE INDEX scheduled_tasks_due_idx
    ON scheduled_tasks (next_run_at)
    WHERE state = 'active' AND next_run_at IS NOT NULL;

-- Listing / cap-counting per owner. Partial index so cancelled / done
-- rows don't bloat the index — list_for_agent and active_count_for_agent
-- both filter on state = 'active'.
CREATE INDEX scheduled_tasks_owner_active_idx
    ON scheduled_tasks (owner_agent_id)
    WHERE state = 'active';
