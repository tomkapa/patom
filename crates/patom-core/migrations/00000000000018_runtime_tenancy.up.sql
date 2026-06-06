-- Runtime tenancy retrofit. Follows the `mcp_servers` (migration 14),
-- `agents` (15), `sessions` (16), and `memory` (17) pattern: every
-- runtime table gains a denormalised `org_id`, a BEFORE-trigger that
-- pins the column to its parent's org (a CHECK can't reference another
-- row), ENABLE + FORCE row level security, and one
-- `<tbl>_org_isolation` policy keyed on `app_user_is_member(org_id)`.
--
-- Tables retrofitted:
--
--   * prompt_requests          — queue rows
--   * prompt_request_dags      — DAG-wide turn budget
--   * session_leases           — at-most-one-lease-per-session
--   * session_turn_seq         — per-session monotonic counter
--   * prompt_response_chunks   — published SSE chunks
--   * prompt_response_streams  — per-request next-seq + closed flag
--
-- `prompt_requests.idempotency_key` becomes per-org unique: different
-- orgs can re-use the same key. The queue's idempotency lookup is
-- already inside the same transaction that resolves the session, so
-- scoping the unique constraint to `(org_id, idempotency_key)` falls
-- out of the same row insert — no separate code path to update.
--
-- Worker pool context: the queue's claim path runs privileged (it
-- scans across all orgs to find work), but joins `prompt_requests →
-- sessions` to pick up `org_id` + `created_by_user_id` so the worker
-- can open a `begin_as_user` turn-tx where supported. The denormalised
-- `org_id` on every runtime row is the load-bearing tenant key — even
-- when a write runs privileged (queue infra), the parity triggers
-- backstop drift and the RLS policy filters reads from the HTTP side.
--
-- Pre-launch: NOT NULL with no backfill (see `feedback_no_backcompat`).
-- Existing dev rows must be wiped before applying; the test schema is
-- minted fresh per test so the test path is naturally unaffected.

-- ───────────────────────────────────────────────────────────────────────────
-- Shared parity-trigger functions. Each runtime table hangs off a
-- different parent (`sessions` for prompt_requests / leases / turn_seq;
-- `prompt_requests` for dags / response chunks / streams), so we need
-- two helpers — one per parent column shape — and reuse them across
-- the tables that share that shape. Same idiom as
-- `enforce_memory_row_parent_org` in migration 17.
-- ───────────────────────────────────────────────────────────────────────────

CREATE OR REPLACE FUNCTION enforce_runtime_row_parent_session_org() RETURNS TRIGGER
    LANGUAGE plpgsql AS $$
DECLARE
    parent_org UUID;
BEGIN
    SELECT org_id INTO parent_org FROM sessions WHERE id = NEW.session_id;
    IF parent_org IS NULL THEN
        RAISE EXCEPTION
            '%.session_id % references missing session',
            TG_TABLE_NAME, NEW.session_id;
    END IF;
    IF parent_org <> NEW.org_id THEN
        RAISE EXCEPTION
            '%.org_id % does not match parent session % org %',
            TG_TABLE_NAME, NEW.org_id, NEW.session_id, parent_org;
    END IF;
    RETURN NEW;
END
$$;

CREATE OR REPLACE FUNCTION enforce_runtime_row_parent_request_org() RETURNS TRIGGER
    LANGUAGE plpgsql AS $$
DECLARE
    parent_org UUID;
    parent_col TEXT;
BEGIN
    -- Both `prompt_request_dags.root_request_id` and
    -- `prompt_response_chunks.request_id` / `prompt_response_streams.request_id`
    -- point at `prompt_requests.id`. The trigger is registered with a
    -- column-name argument so one function covers both shapes.
    parent_col := TG_ARGV[0];
    EXECUTE format(
        'SELECT org_id FROM prompt_requests WHERE id = $1.%I',
        parent_col
    )
    INTO parent_org USING NEW;
    IF parent_org IS NULL THEN
        RAISE EXCEPTION
            '%.% % references missing prompt_request',
            TG_TABLE_NAME, parent_col,
            (SELECT row_to_json(NEW) ->> parent_col);
    END IF;
    IF parent_org <> NEW.org_id THEN
        RAISE EXCEPTION
            '%.org_id % does not match parent prompt_request % org %',
            TG_TABLE_NAME, NEW.org_id,
            (SELECT row_to_json(NEW) ->> parent_col),
            parent_org;
    END IF;
    RETURN NEW;
END
$$;

-- ───────────────────────────────────────────────────────────────────────────
-- prompt_requests
-- ───────────────────────────────────────────────────────────────────────────

ALTER TABLE prompt_requests
    ADD COLUMN org_id UUID NOT NULL REFERENCES organizations(id) ON DELETE CASCADE;

-- Per-org idempotency: different orgs may reuse the same client key.
-- The queue's `enqueue` looks up by `(idempotency_key)` today — the
-- callsite is updated in the same PR to scope the lookup by org_id so
-- the lookup matches the new unique constraint.
ALTER TABLE prompt_requests DROP CONSTRAINT prompt_requests_idempotency_key_key;
ALTER TABLE prompt_requests
    ADD CONSTRAINT prompt_requests_org_idempotency_key_key
    UNIQUE (org_id, idempotency_key);

-- Pending-status partial index leads with `org_id` so future per-org
-- dispatcher scans can use it without a separate index. Today's claim
-- path runs privileged and scans across all orgs; the `org_id`-leading
-- order is forwards-compatible.
DROP INDEX IF EXISTS prompt_requests_pending_idx;
CREATE INDEX prompt_requests_pending_idx
    ON prompt_requests (org_id, session_id, created_at)
    WHERE status = 'pending';

CREATE TRIGGER prompt_requests_enforce_org
    BEFORE INSERT OR UPDATE OF org_id, session_id ON prompt_requests
    FOR EACH ROW
    EXECUTE FUNCTION enforce_runtime_row_parent_session_org();

ALTER TABLE prompt_requests ENABLE ROW LEVEL SECURITY;
ALTER TABLE prompt_requests FORCE ROW LEVEL SECURITY;
CREATE POLICY prompt_requests_org_isolation ON prompt_requests
    FOR ALL TO PUBLIC
    USING      (app_user_is_member(org_id))
    WITH CHECK (app_user_is_member(org_id));

-- ───────────────────────────────────────────────────────────────────────────
-- prompt_request_dags
-- ───────────────────────────────────────────────────────────────────────────

ALTER TABLE prompt_request_dags
    ADD COLUMN org_id UUID NOT NULL REFERENCES organizations(id) ON DELETE CASCADE;

CREATE INDEX prompt_request_dags_org_idx ON prompt_request_dags (org_id);

CREATE TRIGGER prompt_request_dags_enforce_org
    BEFORE INSERT OR UPDATE OF org_id, root_request_id ON prompt_request_dags
    FOR EACH ROW
    EXECUTE FUNCTION enforce_runtime_row_parent_request_org('root_request_id');

ALTER TABLE prompt_request_dags ENABLE ROW LEVEL SECURITY;
ALTER TABLE prompt_request_dags FORCE ROW LEVEL SECURITY;
CREATE POLICY prompt_request_dags_org_isolation ON prompt_request_dags
    FOR ALL TO PUBLIC
    USING      (app_user_is_member(org_id))
    WITH CHECK (app_user_is_member(org_id));

-- ───────────────────────────────────────────────────────────────────────────
-- session_leases
-- ───────────────────────────────────────────────────────────────────────────

ALTER TABLE session_leases
    ADD COLUMN org_id UUID NOT NULL REFERENCES organizations(id) ON DELETE CASCADE;

CREATE INDEX session_leases_org_idx ON session_leases (org_id);

CREATE TRIGGER session_leases_enforce_org
    BEFORE INSERT OR UPDATE OF org_id, session_id ON session_leases
    FOR EACH ROW
    EXECUTE FUNCTION enforce_runtime_row_parent_session_org();

ALTER TABLE session_leases ENABLE ROW LEVEL SECURITY;
ALTER TABLE session_leases FORCE ROW LEVEL SECURITY;
CREATE POLICY session_leases_org_isolation ON session_leases
    FOR ALL TO PUBLIC
    USING      (app_user_is_member(org_id))
    WITH CHECK (app_user_is_member(org_id));

-- ───────────────────────────────────────────────────────────────────────────
-- session_turn_seq
-- ───────────────────────────────────────────────────────────────────────────

ALTER TABLE session_turn_seq
    ADD COLUMN org_id UUID NOT NULL REFERENCES organizations(id) ON DELETE CASCADE;

CREATE INDEX session_turn_seq_org_idx ON session_turn_seq (org_id);

CREATE TRIGGER session_turn_seq_enforce_org
    BEFORE INSERT OR UPDATE OF org_id, session_id ON session_turn_seq
    FOR EACH ROW
    EXECUTE FUNCTION enforce_runtime_row_parent_session_org();

ALTER TABLE session_turn_seq ENABLE ROW LEVEL SECURITY;
ALTER TABLE session_turn_seq FORCE ROW LEVEL SECURITY;
CREATE POLICY session_turn_seq_org_isolation ON session_turn_seq
    FOR ALL TO PUBLIC
    USING      (app_user_is_member(org_id))
    WITH CHECK (app_user_is_member(org_id));

-- ───────────────────────────────────────────────────────────────────────────
-- prompt_response_chunks
-- ───────────────────────────────────────────────────────────────────────────

ALTER TABLE prompt_response_chunks
    ADD COLUMN org_id UUID NOT NULL REFERENCES organizations(id) ON DELETE CASCADE;

CREATE INDEX prompt_response_chunks_org_idx ON prompt_response_chunks (org_id);

CREATE TRIGGER prompt_response_chunks_enforce_org
    BEFORE INSERT OR UPDATE OF org_id, request_id ON prompt_response_chunks
    FOR EACH ROW
    EXECUTE FUNCTION enforce_runtime_row_parent_request_org('request_id');

ALTER TABLE prompt_response_chunks ENABLE ROW LEVEL SECURITY;
ALTER TABLE prompt_response_chunks FORCE ROW LEVEL SECURITY;
CREATE POLICY prompt_response_chunks_org_isolation ON prompt_response_chunks
    FOR ALL TO PUBLIC
    USING      (app_user_is_member(org_id))
    WITH CHECK (app_user_is_member(org_id));

-- ───────────────────────────────────────────────────────────────────────────
-- prompt_response_streams
-- ───────────────────────────────────────────────────────────────────────────

ALTER TABLE prompt_response_streams
    ADD COLUMN org_id UUID NOT NULL REFERENCES organizations(id) ON DELETE CASCADE;

CREATE INDEX prompt_response_streams_org_idx ON prompt_response_streams (org_id);

CREATE TRIGGER prompt_response_streams_enforce_org
    BEFORE INSERT OR UPDATE OF org_id, request_id ON prompt_response_streams
    FOR EACH ROW
    EXECUTE FUNCTION enforce_runtime_row_parent_request_org('request_id');

ALTER TABLE prompt_response_streams ENABLE ROW LEVEL SECURITY;
ALTER TABLE prompt_response_streams FORCE ROW LEVEL SECURITY;
CREATE POLICY prompt_response_streams_org_isolation ON prompt_response_streams
    FOR ALL TO PUBLIC
    USING      (app_user_is_member(org_id))
    WITH CHECK (app_user_is_member(org_id));
