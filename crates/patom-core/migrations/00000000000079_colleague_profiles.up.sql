-- Per-person profile board (doc/colleague-profiles-and-search-plan.md, issue #183).
--
-- `colleague_profiles` is the org-SHARED "who they are" board: one row per
-- colleague carrying durable role/expertise/preferences plus an embedding so a
-- human can be found by `search_colleague` the way an agent is found by its
-- `agents.description_embedding`. This is distinct from `agent_memories`
-- (kind='collaborator'), which is per-agent and PRIVATE ("what *I* learned").
--
-- Keyed by `colleague_id` (already org-scoped via `colleagues`) so the table can
-- later hold agent-override rows without a schema change; #183 mints rows for
-- humans only. `org_id` is denormalised for RLS + cheap org-scoped scans.
--
-- `embedding` is NULL until the first `profile_write` — search skips NULLs, so a
-- human is invisible to discovery until profiled (the hook the HRM follow-on
-- closes). `profile_text` is the composed embedding source (role+expertise+
-- preferences flattened); the structured columns render cleanly in the prompt.

CREATE TABLE colleague_profiles (
    colleague_id         UUID PRIMARY KEY REFERENCES colleagues(id) ON DELETE CASCADE,
    org_id               UUID NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    role                 TEXT NULL CHECK (role        IS NULL OR octet_length(role)        BETWEEN 1 AND 200),
    expertise            TEXT NULL CHECK (expertise   IS NULL OR octet_length(expertise)   BETWEEN 1 AND 2048),
    preferences          TEXT NULL CHECK (preferences IS NULL OR octet_length(preferences) BETWEEN 1 AND 2048),
    -- Composed, non-empty embedding source. Always rewritten alongside the
    -- structured fields, so it can never be longer than their combined caps.
    profile_text         TEXT NOT NULL CHECK (octet_length(profile_text) BETWEEN 1 AND 5120),
    -- Same 1536 dimension as agents/agent_memories so a cross-source UNION ranks
    -- by comparable cosine distance. NULL until first embed (degraded layer).
    embedding            vector(1536) NULL,
    -- Provenance: who last wrote this row (HRM vs self vs peer). SET NULL so a
    -- departed colleague doesn't cascade away everyone's profiles.
    updated_by_colleague UUID NULL REFERENCES colleagues(id) ON DELETE SET NULL,
    created_at           TIMESTAMPTZ NOT NULL,
    updated_at           TIMESTAMPTZ NOT NULL
);

-- RLS predicate and the org-scoped search UNION are both org-keyed.
CREATE INDEX colleague_profiles_org_idx ON colleague_profiles (org_id);

-- Org isolation, mirroring `colleagues` (migration 58): ENABLE + FORCE so even a
-- table owner is subject to the policy. Reads on the prompt path run privileged
-- (they join `users`, REVOKEd from `patom_app`); this policy guards member-context
-- access and is the WITH CHECK backstop for writes.
ALTER TABLE colleague_profiles ENABLE ROW LEVEL SECURITY;
ALTER TABLE colleague_profiles FORCE ROW LEVEL SECURITY;
CREATE POLICY colleague_profiles_org_isolation ON colleague_profiles
    FOR ALL TO PUBLIC
    USING      (app_user_is_member(org_id))
    WITH CHECK (app_user_is_member(org_id));

-- HNSW cosine index for the human side of the search UNION. Partial on the
-- NOT-NULL predicate the search query uses, so unprofiled humans cost nothing.
CREATE INDEX colleague_profiles_embedding_hnsw
    ON colleague_profiles USING hnsw (embedding vector_cosine_ops)
    WHERE embedding IS NOT NULL;

-- ───────────────────────────────────────────────────────────────────────────
-- Latent fix: `agents.description_embedding` was added UNSIZED (`VECTOR`, no
-- dimension) in migration 13 and never indexed, so the agent side of the search
-- UNION seq-scans. pgvector needs a fixed dimension for an HNSW index — size the
-- column to 1536 (the existing rows already came from the same 1536-dim
-- provider) then build the partial cosine index that matches `<=>`.
-- ───────────────────────────────────────────────────────────────────────────
ALTER TABLE agents
    ALTER COLUMN description_embedding TYPE vector(1536)
        USING description_embedding::vector(1536);

CREATE INDEX agents_description_embedding_hnsw
    ON agents USING hnsw (description_embedding vector_cosine_ops)
    WHERE description_embedding IS NOT NULL;
