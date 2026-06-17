-- tool_artifacts — produce-time offload store for heavy tool results (#185).
--
-- Patom has no agent filesystem, so this table is the database-backed "scratch
-- file" every other agent harness leans on: when a tool result exceeds
-- TOOL_RESULT_REDUCE_THRESHOLD the full body is written here and the visible
-- feed result is reduced (paginate preview or cheap-model summary) carrying only
-- the content-addressed `handle`. `read_artifact(handle, ...)` recovers exact
-- slices on demand, so reduction is lossless. Companion to #182.
--
-- `handle` is the SHA-256 of `full_body` (content-addressed), which makes the
-- write idempotent: a turn that re-runs the same tool after a lease expiry
-- recomputes the same handle and the ON CONFLICT DO NOTHING is a no-op.
--
-- Retention is cascade-only in v1: artifacts are removed when their org or
-- owning agent is deleted (no TTL sweeper yet).
CREATE TABLE tool_artifacts (
    -- Content address: lowercase SHA-256 hex of `full_body` (64 chars).
    handle      TEXT        NOT NULL,
    org_id      UUID        NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    -- The full, unreduced tool-result body (the offloaded bytes).
    full_body   TEXT        NOT NULL,
    -- chars/4 token estimate of `full_body`, for the saturation metric.
    tokens      INTEGER     NOT NULL CHECK (tokens >= 0),
    -- Producing tool's name, for the `patom.tool_result.reduced` metric attrs.
    tool_name   TEXT        NOT NULL,
    -- Owner agent — artifact is cleaned up with the agent. NULL on the
    -- background path with no agent-participation row.
    agent_id    UUID        REFERENCES agents(id) ON DELETE CASCADE,
    -- agent_thread_state.id of the producing turn (audit/GC; no FK — the state
    -- row is itself derived and may be compacted away independently).
    state_id    UUID,
    request_id  UUID        NOT NULL,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    -- Content-addressed, write-once per org.
    PRIMARY KEY (org_id, handle)
);

-- Org isolation, mirroring thread_compactions / the rest of the schema. The
-- read path also binds org explicitly (memory: rls-gates-membership-not-active-org).
ALTER TABLE tool_artifacts ENABLE ROW LEVEL SECURITY;
ALTER TABLE tool_artifacts FORCE ROW LEVEL SECURITY;
CREATE POLICY tool_artifacts_org_isolation ON tool_artifacts
    FOR ALL TO PUBLIC
    USING      (app_user_is_member(org_id))
    WITH CHECK (app_user_is_member(org_id));

-- Cascade-cleanup helper: drop an agent's artifacts efficiently when it is
-- deleted, and support any future per-agent GC sweep.
CREATE INDEX tool_artifacts_agent_idx ON tool_artifacts (agent_id);
