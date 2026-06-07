-- Colleagues directory: humans and agents as one addressable roster.
--
-- A colleague is one end of a session — a distinct, named identity backed by
-- either a human (`user_id`) or an agent (`agent_id`). The agent perceives both
-- as the same kind of coworker. The synthetic *System* end of a
-- reflection/resolution session is the NULL-reference convention, never a row,
-- so `kind` is ('human','agent') only — there is no 'system' kind.
--
-- `display_name` is intentionally NOT stored here: the roster resolves it by
-- joining `agents.name` / `users.display_name` at read time, so a rename never
-- leaves a stale copy behind.

CREATE TABLE colleagues (
    id         UUID PRIMARY KEY,
    org_id     UUID NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    kind       TEXT NOT NULL CHECK (kind IN ('human', 'agent')),
    -- Satellite FKs: exactly the one matching `kind` is non-null. Hard deletes
    -- of the backing row cascade the colleague away.
    user_id    UUID NULL REFERENCES users(id)  ON DELETE CASCADE,
    agent_id   UUID NULL REFERENCES agents(id) ON DELETE CASCADE,
    created_at TIMESTAMPTZ NOT NULL,
    updated_at TIMESTAMPTZ NOT NULL,
    -- kind ⇔ satellite invariant; mirrors the Rust `Colleague::try_new` check.
    CONSTRAINT colleagues_kind_satellite CHECK (
        (kind = 'human' AND user_id IS NOT NULL AND agent_id IS NULL)
     OR (kind = 'agent' AND agent_id IS NOT NULL AND user_id IS NULL)
    )
);

-- One colleague per (org, human) and per (org, agent). Partial so the NULL
-- satellite of the other kind never collides — these indexes also serve as
-- the conflict targets for the `ON CONFLICT DO NOTHING` mint idempotency below.
CREATE UNIQUE INDEX colleagues_org_user_unique  ON colleagues (org_id, user_id)  WHERE user_id  IS NOT NULL;
CREATE UNIQUE INDEX colleagues_org_agent_unique ON colleagues (org_id, agent_id) WHERE agent_id IS NOT NULL;
-- RLS predicate and roster scan are both org-keyed.
CREATE INDEX colleagues_org_idx ON colleagues (org_id);

-- Backfill BEFORE enabling RLS so a non-superuser migration role isn't blocked
-- by the org-isolation WITH CHECK (app.user_id is unset during migration). On a
-- fresh/reset DB these are no-ops; on a populated DB they mint one colleague per
-- existing agent and per existing membership. Idempotency is delegated to the
-- partial unique indexes via ON CONFLICT — no subquery per row.
INSERT INTO colleagues (id, org_id, kind, agent_id, created_at, updated_at)
SELECT gen_random_uuid(), a.org_id, 'agent', a.id, a.created_at, a.created_at
  FROM agents a
ON CONFLICT (org_id, agent_id) WHERE agent_id IS NOT NULL DO NOTHING;

INSERT INTO colleagues (id, org_id, kind, user_id, created_at, updated_at)
SELECT gen_random_uuid(), m.org_id, 'human', m.user_id, m.created_at, m.created_at
  FROM org_members m
ON CONFLICT (org_id, user_id) WHERE user_id IS NOT NULL DO NOTHING;

ALTER TABLE colleagues ENABLE ROW LEVEL SECURITY;
ALTER TABLE colleagues FORCE ROW LEVEL SECURITY;
CREATE POLICY colleagues_org_isolation ON colleagues
    FOR ALL TO PUBLIC
    USING      (app_user_is_member(org_id))
    WITH CHECK (app_user_is_member(org_id));

-- Eager minting: every agent and every membership grant gets a colleague,
-- in the same transaction as the parent insert, so the directory can never
-- silently miss a member. One parametric function handles both shapes; the
-- branch is on TG_ARGV[0] (the colleague kind), following the precedent set
-- by `enforce_runtime_row_parent_request_org()` in migration 18. The
-- colleague's timestamps flow from the parent row (no NOW() — §11). The
-- partial unique indexes drive idempotency via ON CONFLICT — re-grants are
-- a no-op without a subquery.
CREATE FUNCTION mint_colleague() RETURNS TRIGGER LANGUAGE plpgsql AS $$
DECLARE
    k TEXT := TG_ARGV[0];
BEGIN
    IF k = 'agent' THEN
        INSERT INTO colleagues (id, org_id, kind, agent_id, created_at, updated_at)
        VALUES (gen_random_uuid(), NEW.org_id, 'agent', NEW.id,
                NEW.created_at, NEW.created_at)
        ON CONFLICT (org_id, agent_id) WHERE agent_id IS NOT NULL DO NOTHING;
    ELSIF k = 'human' THEN
        INSERT INTO colleagues (id, org_id, kind, user_id, created_at, updated_at)
        VALUES (gen_random_uuid(), NEW.org_id, 'human', NEW.user_id,
                NEW.created_at, NEW.created_at)
        ON CONFLICT (org_id, user_id) WHERE user_id IS NOT NULL DO NOTHING;
    ELSE
        RAISE EXCEPTION 'mint_colleague: unknown kind %', k;
    END IF;
    RETURN NEW;
END $$;

CREATE TRIGGER agents_mint_colleague
    AFTER INSERT ON agents
    FOR EACH ROW EXECUTE FUNCTION mint_colleague('agent');

CREATE TRIGGER org_members_mint_colleague
    AFTER INSERT ON org_members
    FOR EACH ROW EXECUTE FUNCTION mint_colleague('human');
