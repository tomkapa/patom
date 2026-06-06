-- Sessions tenancy retrofit. Follows the `mcp_servers` (migration 14)
-- and `agents` (migration 15) pattern: add `org_id`, enable + force RLS,
-- install an isolation policy. Sessions additionally pick up:
--
-- * `created_by_user_id` — the human at the DAG root. The worker pool
--   retrofit will set `app.user_id` from this column when claiming a
--   session so tenant-scoped reads inside the turn run against the
--   right principal.
-- * A `BEFORE INSERT OR UPDATE` trigger that pins every child session
--   to its parent's org (a CHECK can't reference another row).
-- * Re-keyed `sessions_dag_pair_unique` and `sessions_root_idx` so the
--   `(org_id, root_request_id)` prefix scopes DAG-anchored scans per
--   tenant — quiescence checks and the reflection scheduler all
--   benefit.
--
-- `session_messages` denormalises `org_id` so RLS on that table is
-- self-contained (no JOIN to `sessions` in the policy hot path). A
-- companion trigger raises if the inserted `org_id` doesn't match the
-- parent session's, so cross-org rows can't sneak in via raw SQL even
-- with the app layer correctly passing the parent's org through.
--
-- Pre-launch: NOT NULL with no backfill (see `feedback_no_backcompat`).
-- Existing dev rows must be wiped before applying; the test schema is
-- minted fresh per test so the test path is naturally unaffected.

-- ───────────────────────────────────────────────────────────────────────────
-- sessions
-- ───────────────────────────────────────────────────────────────────────────

ALTER TABLE sessions
    ADD COLUMN org_id             UUID NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    ADD COLUMN created_by_user_id UUID NOT NULL REFERENCES users(id);

-- DAG-pair uniqueness now scoped per org. NULLS NOT DISTINCT keeps the
-- human-side NULL agent ids deduping correctly (same rationale as
-- migration 4).
DROP INDEX IF EXISTS sessions_dag_pair_unique;
CREATE UNIQUE INDEX sessions_dag_pair_unique
    ON sessions (org_id, root_request_id,
                 participant_a_kind, participant_a_agent_id,
                 participant_b_kind, participant_b_agent_id)
    NULLS NOT DISTINCT;

-- Quiescence / DAG dispatch scans lead with `org_id` so the index is
-- usable for the future per-org dispatcher pass too.
DROP INDEX IF EXISTS sessions_root_idx;
CREATE INDEX sessions_root_idx ON sessions (org_id, root_request_id);

-- Cross-org child sessions are forbidden — a child must inherit its
-- parent's org. CHECK constraints can't reference other rows, so a
-- trigger enforces it.
CREATE OR REPLACE FUNCTION enforce_sessions_parent_org() RETURNS TRIGGER
    LANGUAGE plpgsql AS $$
DECLARE
    parent_org UUID;
BEGIN
    IF NEW.parent_session_id IS NULL THEN
        RETURN NEW;
    END IF;
    SELECT org_id INTO parent_org FROM sessions WHERE id = NEW.parent_session_id;
    IF parent_org IS NULL THEN
        RAISE EXCEPTION
            'sessions.parent_session_id % references missing session',
            NEW.parent_session_id;
    END IF;
    IF parent_org <> NEW.org_id THEN
        RAISE EXCEPTION
            'sessions.org_id % does not match parent session % org %',
            NEW.org_id, NEW.parent_session_id, parent_org;
    END IF;
    RETURN NEW;
END
$$;

CREATE TRIGGER sessions_enforce_parent_org
    BEFORE INSERT OR UPDATE OF org_id, parent_session_id ON sessions
    FOR EACH ROW
    EXECUTE FUNCTION enforce_sessions_parent_org();

ALTER TABLE sessions ENABLE ROW LEVEL SECURITY;
ALTER TABLE sessions FORCE ROW LEVEL SECURITY;
CREATE POLICY sessions_org_isolation ON sessions
    FOR ALL TO PUBLIC
    USING      (app_user_is_member(org_id))
    WITH CHECK (app_user_is_member(org_id));

-- ───────────────────────────────────────────────────────────────────────────
-- session_messages
-- ───────────────────────────────────────────────────────────────────────────

ALTER TABLE session_messages
    ADD COLUMN org_id UUID NOT NULL REFERENCES organizations(id) ON DELETE CASCADE;

CREATE INDEX session_messages_org_idx ON session_messages (org_id);

-- Denormalised `org_id` must match the parent session's. A trigger
-- enforces it because CHECK constraints can't reference other rows.
CREATE OR REPLACE FUNCTION enforce_session_messages_org() RETURNS TRIGGER
    LANGUAGE plpgsql AS $$
DECLARE
    parent_org UUID;
BEGIN
    SELECT org_id INTO parent_org FROM sessions WHERE id = NEW.session_id;
    IF parent_org IS NULL THEN
        RAISE EXCEPTION
            'session_messages.session_id % references missing session',
            NEW.session_id;
    END IF;
    IF parent_org <> NEW.org_id THEN
        RAISE EXCEPTION
            'session_messages.org_id % does not match parent session % org %',
            NEW.org_id, NEW.session_id, parent_org;
    END IF;
    RETURN NEW;
END
$$;

CREATE TRIGGER session_messages_enforce_org
    BEFORE INSERT OR UPDATE OF org_id, session_id ON session_messages
    FOR EACH ROW
    EXECUTE FUNCTION enforce_session_messages_org();

ALTER TABLE session_messages ENABLE ROW LEVEL SECURITY;
ALTER TABLE session_messages FORCE ROW LEVEL SECURITY;
CREATE POLICY session_messages_org_isolation ON session_messages
    FOR ALL TO PUBLIC
    USING      (app_user_is_member(org_id))
    WITH CHECK (app_user_is_member(org_id));
