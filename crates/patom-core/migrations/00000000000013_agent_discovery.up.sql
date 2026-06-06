-- Agent discovery & network (doc/agent_discovery_plan.md).
--
-- Three schema changes land together:
--
-- 1. `agents.description` — operator-curated, model-facing one-sentence
--    blurb describing what the agent is for. Distinct from `system_prompt`:
--    `description` is for *being found* (embedded for `search_agents`);
--    `system_prompt` is for *being the agent*. Required, non-empty.
-- 2. `agents.description_embedding` — pgvector column backing the
--    `search_agents` similarity search. Nullable for the same reason
--    `agent_memories.embedding` is nullable (embeddings come from an
--    injected provider; backfill happens on first write/seed).
-- 3. Case-insensitive global uniqueness on `name`. Roles, not personas
--    — two `designer` rows is operator error. When tenancy lands the
--    uniqueness becomes tenant-scoped in the same migration that
--    introduces the boundary.
--
-- Pre-launch, greenfield: NOT NULL with no default and no backfill step.
-- The default agent's description is supplied by the seed in
-- `crate::app::DEFAULT_AGENT_DESCRIPTION`. If a dev DB has existing
-- rows, drop them before applying.

ALTER TABLE agents
    ADD COLUMN description TEXT NOT NULL
               CHECK (octet_length(description) BETWEEN 1 AND 512),
    ADD COLUMN description_embedding VECTOR NULL;

-- Names are role-shaped and globally unique on `lower(name)`. The
-- partial-unique-style index here uses an expression index so the
-- uniqueness scope matches the agent_discovery_plan.md §6 wording
-- exactly. Drops the previous "lots of `designer`s would just work"
-- assumption — that's now operator error caught at insert time.
CREATE UNIQUE INDEX agents_name_lower_unique
    ON agents ((lower(name)));

-- ───────────────────────────────────────────────────────────────────────────
-- agent_memories.kind — add `collaborator` to the allowed label set
-- (doc/agent_discovery_plan.md §4). The existing four kinds stay; this is
-- additive so existing rows are unaffected.
-- ───────────────────────────────────────────────────────────────────────────
ALTER TABLE agent_memories
    DROP CONSTRAINT agent_memories_kind_check;

ALTER TABLE agent_memories
    ADD CONSTRAINT agent_memories_kind_check
        CHECK (kind IN ('self','other','collaborator','procedure','open'));
