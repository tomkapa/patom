-- Per-session agent todo list. One row per session — the todo tool
-- writes the whole list atomically (TodoWrite semantics), so a wide
-- JSONB column keyed by `session_id` is the natural shape: no per-item
-- ordering column, no per-item DELETE/INSERT churn on every overwrite.
--
-- Scoping: per-session (the closest Relay analogue to a Claude Code
-- conversation). A new session starts with a clean list; the same
-- session's list survives across turns and re-runs of the same
-- `prompt_request` lineage, because the row is keyed on `session_id`.
--
-- `item_count` is denormalised so the CHECK constraint backs the §5
-- hard cap in addition to the app-side `TodoList::try_from` invariant.
-- `updated_in_request_id` is audit-only (which turn last wrote this
-- list); kept as a bare UUID — adding an FK to `prompt_requests` adds
-- ordering complexity during session-cascade deletes for no real
-- referential benefit (the row is immutable history once written).
--
-- Tenancy: same two-layer pattern as `session_messages` (migration 16):
-- denormalised `org_id`, RLS policy via `app_user_is_member(org_id)`,
-- and a BEFORE INSERT/UPDATE trigger that pins `org_id` to the parent
-- session's. Pre-launch, NOT NULL with no backfill (see
-- `feedback_no_backcompat`).

CREATE TABLE session_todos (
    session_id            UUID        PRIMARY KEY REFERENCES sessions(id) ON DELETE CASCADE,
    org_id                UUID        NOT NULL    REFERENCES organizations(id) ON DELETE CASCADE,
    items                 JSONB       NOT NULL,
    item_count            SMALLINT    NOT NULL    CHECK (item_count BETWEEN 0 AND 50),
    updated_at            TIMESTAMPTZ NOT NULL,
    updated_in_request_id UUID        NOT NULL
);

CREATE INDEX session_todos_org_idx ON session_todos (org_id);

-- Defence in depth: every write into `session_todos` must carry the
-- parent session's `org_id`. The app passes this through `ctx.org_id`
-- but a foreign-org write would slip past the FK alone, so a trigger
-- equality-checks against `sessions.org_id`.
CREATE OR REPLACE FUNCTION enforce_session_todos_parent_org() RETURNS TRIGGER
    LANGUAGE plpgsql AS $$
DECLARE
    parent_org UUID;
BEGIN
    SELECT org_id INTO parent_org FROM sessions WHERE id = NEW.session_id;
    IF parent_org IS NULL THEN
        RAISE EXCEPTION
            'session_todos.session_id % references missing session',
            NEW.session_id;
    END IF;
    IF parent_org <> NEW.org_id THEN
        RAISE EXCEPTION
            'session_todos.org_id % does not match parent session % org %',
            NEW.org_id, NEW.session_id, parent_org;
    END IF;
    RETURN NEW;
END
$$;

CREATE TRIGGER session_todos_enforce_org
    BEFORE INSERT OR UPDATE OF org_id, session_id ON session_todos
    FOR EACH ROW
    EXECUTE FUNCTION enforce_session_todos_parent_org();

ALTER TABLE session_todos ENABLE ROW LEVEL SECURITY;
ALTER TABLE session_todos FORCE  ROW LEVEL SECURITY;
CREATE POLICY session_todos_org_isolation ON session_todos
    FOR ALL TO PUBLIC
    USING      (app_user_is_member(org_id))
    WITH CHECK (app_user_is_member(org_id));
