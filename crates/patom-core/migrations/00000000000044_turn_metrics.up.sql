-- turn_metrics — one row per LLM provider call (normal, reflection, resolution).
--
-- Today `prompt_requests` records that a turn happened (status, attempts,
-- failure) but not what it *cost* — token counts and model timing are
-- discarded once the provider call returns. This table is the columnar
-- per-turn metrics anchor that powers the Logs & Metrics tab
-- (doc/logs_metrics_tab.md §4.2).
--
-- One INSERT per turn, ~80 bytes per row. Aggregations live in SQL — the
-- HTTP layer never ships raw rows to the chart.
--
-- Pre-launch single-step migration: NOT NULL with no backfill. Dev DBs are
-- wiped before applying (feedback_no_backcompat).

CREATE TABLE turn_metrics (
    request_id            UUID PRIMARY KEY REFERENCES prompt_requests(id) ON DELETE CASCADE,
    org_id                UUID NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    session_id            UUID NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    agent_id              UUID NOT NULL REFERENCES agents(id),
    prompt_version_id     UUID NOT NULL REFERENCES agent_prompt_versions(id),
    -- Mirrors prompt_requests.kind ("normal" | "reflection" | "resolution").
    -- Denormalised so the chart can group without joining prompt_requests.
    kind                  TEXT NOT NULL
                           CHECK (kind IN ('normal','reflection','resolution')),
    model                 TEXT NOT NULL
                           CHECK (octet_length(model) BETWEEN 1 AND 128),
    provider              TEXT NOT NULL
                           CHECK (provider IN ('anthropic','openai','deepseek')),
    input_tokens          INTEGER NOT NULL CHECK (input_tokens  >= 0),
    output_tokens         INTEGER NOT NULL CHECK (output_tokens >= 0),
    -- Nullable: provider may omit (only Anthropic reports caching today).
    cache_creation_tokens INTEGER CHECK (cache_creation_tokens IS NULL OR cache_creation_tokens >= 0),
    cache_read_tokens     INTEGER CHECK (cache_read_tokens     IS NULL OR cache_read_tokens     >= 0),
    duration_ms           INTEGER NOT NULL CHECK (duration_ms >= 0),
    -- end_turn | tool_use | length | other:<provider-detail>
    stop_reason           TEXT NOT NULL
                           CHECK (octet_length(stop_reason) BETWEEN 1 AND 64),
    started_at            TIMESTAMPTZ NOT NULL,
    created_at            TIMESTAMPTZ NOT NULL
);

-- Per-agent time-range scans (chart + timeline) lead with (agent_id, started_at DESC).
CREATE INDEX turn_metrics_agent_idx
    ON turn_metrics (agent_id, started_at DESC);

-- Per-session detail / drawer cross-references.
CREATE INDEX turn_metrics_session_idx
    ON turn_metrics (session_id, started_at DESC);

-- "Compared to what?" slice: every aggregate splits by prompt version.
CREATE INDEX turn_metrics_agent_version_idx
    ON turn_metrics (agent_id, prompt_version_id);

-- Denormalised org_id must match the parent session's. Trigger mirrors
-- enforce_tool_calls_org in migration 25.
CREATE OR REPLACE FUNCTION enforce_turn_metrics_org() RETURNS TRIGGER
    LANGUAGE plpgsql AS $$
DECLARE
    parent_org UUID;
BEGIN
    SELECT org_id INTO parent_org FROM sessions WHERE id = NEW.session_id;
    IF parent_org IS NULL THEN
        RAISE EXCEPTION
            'turn_metrics.session_id % references missing session',
            NEW.session_id;
    END IF;
    IF parent_org <> NEW.org_id THEN
        RAISE EXCEPTION
            'turn_metrics.org_id % does not match parent session % org %',
            NEW.org_id, NEW.session_id, parent_org;
    END IF;
    RETURN NEW;
END
$$;

CREATE TRIGGER turn_metrics_enforce_org
    BEFORE INSERT OR UPDATE OF org_id, session_id ON turn_metrics
    FOR EACH ROW
    EXECUTE FUNCTION enforce_turn_metrics_org();

ALTER TABLE turn_metrics ENABLE ROW LEVEL SECURITY;
ALTER TABLE turn_metrics FORCE ROW LEVEL SECURITY;
CREATE POLICY turn_metrics_org_isolation ON turn_metrics
    FOR ALL TO PUBLIC
    USING      (app_user_is_member(org_id))
    WITH CHECK (app_user_is_member(org_id));
