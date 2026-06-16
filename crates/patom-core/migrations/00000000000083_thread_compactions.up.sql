-- thread_compactions — the rolling per-(thread, agent) context summary (#182).
--
-- A *derived* artifact: the immutable `thread_messages` feed stays the system of
-- record. One row per (thread, agent); `summary` folds in everything up to
-- `covers_through_seq`, and the read path (`context_tail(since = covers_through_seq)`)
-- ships only newer rows verbatim. `failed_attempts`/`cooldown_until` back off a
-- failing summarizer so it degrades to the windowing floor instead of stalling
-- every turn.
CREATE TABLE thread_compactions (
    thread_id          UUID        NOT NULL REFERENCES threads(id) ON DELETE CASCADE,
    agent_id           UUID        NOT NULL REFERENCES agents(id) ON DELETE CASCADE,
    org_id             UUID        NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    -- The structured rolling summary (Facts/Decisions/Constraints/Open items/Progress).
    summary            TEXT        NOT NULL,
    -- Highest feed `seq` folded into `summary` for this (thread, agent).
    covers_through_seq BIGINT      NOT NULL CHECK (covers_through_seq >= 0),
    -- Estimated token cost of `summary` (chars/4), for budgeting the next read.
    summary_tokens     INTEGER     NOT NULL CHECK (summary_tokens >= 0),
    -- Consecutive summarizer failures; reset to 0 on success. Drives the cooldown.
    failed_attempts    INTEGER     NOT NULL DEFAULT 0 CHECK (failed_attempts >= 0),
    -- While `now() < cooldown_until` the read path skips the LLM and serves the
    -- floor + the (stale) summary. NULL = no active cooldown.
    cooldown_until     TIMESTAMPTZ,
    version            INTEGER     NOT NULL DEFAULT 1,
    updated_at         TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (thread_id, agent_id)
);

-- Org isolation, mirroring `thread_messages` / the rest of the schema. The app
-- also pins the active org in the read query (memory: rls-gates-membership-not-active-org).
ALTER TABLE thread_compactions ENABLE ROW LEVEL SECURITY;
ALTER TABLE thread_compactions FORCE ROW LEVEL SECURITY;
CREATE POLICY thread_compactions_org_isolation ON thread_compactions
    FOR ALL TO PUBLIC
    USING      (app_user_is_member(org_id))
    WITH CHECK (app_user_is_member(org_id));

-- Admit the new metering kind. Compaction folds record a `turn_metrics` row
-- under the enclosing turn's `request_id` so summarizer spend is billed to the
-- org (the `prompt_requests` row keeps the turn's own kind, so only this CHECK
-- needs the extra value).
ALTER TABLE turn_metrics DROP CONSTRAINT turn_metrics_kind_check;
ALTER TABLE turn_metrics
    ADD CONSTRAINT turn_metrics_kind_check
    CHECK (kind IN ('normal', 'reflection', 'resolution', 'compaction'));
