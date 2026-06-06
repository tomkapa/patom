-- Agents registry. One row per agent definition; the system prompt that
-- distinguishes one agent from another lives in the `system_prompt` column.
-- Combined with the in-code `<core>` prompt at request time, this is what the
-- model sees as `system` for every turn.
--
-- `is_default` plus the partial unique index `agents_default_unique` enforce
-- "exactly one default agent" — the row picked when a session-create omits an
-- explicit agent_id. The init function in `crate::agents::seed_default` seeds
-- one such row at startup.
--
-- Pre-launch single-step migration: `sessions.agent_id NOT NULL` lands in the
-- same migration as the `agents` table because there is no production data to
-- backfill. If a dev DB has existing rows, drop them before applying.
CREATE TABLE agents (
    id            UUID PRIMARY KEY,
    name          TEXT NOT NULL
                  CHECK (octet_length(name) BETWEEN 1 AND 64),
    system_prompt TEXT NOT NULL
                  CHECK (octet_length(system_prompt) BETWEEN 1 AND 65536),
    is_default    BOOLEAN NOT NULL DEFAULT FALSE,
    created_at    TIMESTAMPTZ NOT NULL,
    updated_at    TIMESTAMPTZ NOT NULL
);

-- Partial unique index: at most one row may have is_default = TRUE. The init
-- seeder serialises its check + insert through `pg_advisory_xact_lock`, so the
-- index is the last line of defence rather than the primary race-killer.
CREATE UNIQUE INDEX agents_default_unique
    ON agents (is_default)
    WHERE is_default;

-- Bind every session to a specific agent. The agent picked at session-create
-- time governs every turn for that session — the worker re-loads the prompt
-- per turn (with a short TTL cache) so an admin edit takes effect within ~60s
-- without rebuilding history.
ALTER TABLE sessions
    ADD COLUMN agent_id UUID NOT NULL REFERENCES agents(id);
