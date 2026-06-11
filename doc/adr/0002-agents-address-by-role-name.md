# ADR-0002 — Agents address each other by role name

- **Status:** Superseded (2026-06-11) — `send_message` now addresses every recipient by a single colleague **id**, surfaced in the `<colleagues>` roster + `<speaking-with>` block (which solve the "model doesn't know ids" problem this ADR cited). The `{kind:"agent",name}` / `{kind:"human"}` receiver sugar is removed; the kind (human vs agent) is derived from the resolved `colleagues` row. Role-name *discovery* (`search_agents`, Collaborator memory) is unaffected — only the `send_message` receiver wire changed.
- **Date:** 2026-05-15
- **Deciders:** core

## Context

An early version of `send_message` required the caller to pass the receiver's `agent_id` — a UUID. The model only knew UUIDs because operators pasted them into the prompt body during scenario authoring. That broke the workplace model the product is built on: a new hire doesn't get handed every coworker's employee number on day one, and a senior colleague isn't summoned by UUID.

We also wanted discovery to be a runtime concern of the agent (who do I delegate to right now?), not a deployment-time concern of the operator (paste all the ids into my role prompt).

## Decision

**Agents address one another by name. Names are role-shaped, snake_case, globally unique on `lower(name)` per org.** Examples: `account_manager`, `brand_strategist`, `copywriter`, `designer`, `project_coordinator`.

- `send_message`'s receiver shape is `{kind: "agent", name: <role_name>}`. The id-based path is removed.
- An always-on `<agents>` block in every agent's system prompt lists the names of every coworker (caller excluded). Names only — no descriptions, no ids.
- A new tool `search_agents(query)` does semantic search over a separate operator-curated `agents.description` field, returning `{name, description}` cards.
- A new memory kind `Collaborator` lets an agent record what it has learned about peers ("for visual mockups, `designer` is the right call — responsive, on-brief").

Discovery happens in four layers, tried in order:

1. **Role prompt** — named procedural peers ("you brief the brand-strategist") wired in by the operator.
2. **`<agents>` index** — the always-on org chart.
3. **`Collaborator` memory** — past delegations the agent has learned to repeat, surfaced via the contextual memory layer.
4. **`search_agents`** — disambiguator for genuinely new tasks.

When no layer yields a recipient, the `<core>` prompt instructs the agent to `send_message` the human asking who should own this — not to improvise.

## Consequences

**What becomes easy:**

- The model addresses peers the way a human would — by role. UUIDs never appear in the prompt or in the tool surface.
- The `<agents>` index is identity-shaped (low cardinality, stable, cheap to cache). Descriptions are fetched on demand, the same pattern deferred tools use for their schemas.
- Stale memory ("delegate to `designer`") self-corrects: if an operator renames `designer`, the next `send_message` returns `unknown agent`, the model retries via `search_agents`, writes a fresh `Collaborator` memory.
- Slack `@mentions` map directly: `<@PatomBot> @designer ...` → the same name resolution.

**What becomes hard:**

- One agent per role per org. Two `designer` rows is operator error — the column has a case-insensitive unique index. If multi-instance roles ever become a real need, the answer is a naming convention (`designer_brand` / `designer_web`) or a separate structured `role` column. Not in scope today.
- Names are now model-facing surface. The earlier invariant that "the model never sees the name, only the resolved `system_prompt`" is dropped.
- The operator-curated `description` is required (NOT NULL, non-empty) — you cannot register an agent into the network without saying what it's for.

**What we live with:**

- The librarian's dedup can produce mild oscillation on stale collaborator memories after a rename. Convergent over time via decay + eviction; not a blocker.

## Alternatives considered

- **Keep UUIDs; teach the model via a `<peers>` block with `{id, name, description}` cards.** Heavy, branchy, doesn't match how humans build networks, and operator-pasted ids leak through prompts. Rejected.
- **Names without descriptions and without `search_agents`.** Discovery dead-ends as soon as a deployment has more than a handful of agents, or when a new task lands that doesn't match any obvious role name. Rejected.
- **Per-tenant uniqueness instead of global.** Tenants weren't yet a concept when this ADR landed. The decision is "globally unique on `lower(name)`" today; when the tenant boundary lands (ADR-0007), uniqueness becomes per-org in the same migration.
