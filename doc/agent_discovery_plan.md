# Agent Discovery & Network

How agents find each other in a patom-rs deployment, and how the
collaborator network grows. Companion to [`memory.md`](./memory.md) and
[`pitch_demo.md`](./pitch_demo.md).

This document is the **design** — what behaviour we are committing to,
which fields and concepts get added, what each layer of the system
prompt is responsible for. Implementation paths (files, migrations,
test plan) land in a follow-up doc.

---

## 1. Motivation

Today's `send_message` requires the caller to supply the receiver's
`agent_id` — a UUID the model only knows because the operator pasted it
into the prompt body. That makes discovery a deployment-time concern of
the operator, not a runtime concern of the agent. It breaks the
"workplace" model the pitch is built around: a new hire doesn't get
handed every coworker's employee number on day one, and a senior
colleague isn't summoned by UUID.

The product premise (per [`pitch_demo.md` §3.6](./pitch_demo.md)) is
that agents behave like real employees. Real employees build their
collaborator network in three layers — what their job description tells
them, what they've learned from working with people, and what they ask
around to find out. Discovery should match that shape.

---

## 2. The four-layer model

The model tries these layers **in order** when it needs to delegate.
Each is cheaper than the next, so the common case never pays for the
uncommon one.

### Layer 1 — The role prompt's named peers

Operator-wired procedural hops. The `account_manager`'s role prompt
literally says "you brief the `brand_strategist`; the `copywriter` and
`designer` work under their direction; status reports go to
`project_coordinator`." These are not discovered; they are known by
virtue of the agent's role definition. The model addresses them by name
without any tool call.

**This layer is free.** It is also the highest-frequency layer in
practice — most delegations are the same procedural hops over and over.

### Layer 2 — The `<agents>` name index

A flat, alphabetised list of every agent's name (role-shaped, e.g.
`copywriter, designer, project_coordinator, translator`), injected into
the system prompt alongside `<core>` / `<role>` / `<memory>`. **The
caller is excluded.** Names only — no descriptions, no ids.

This is the org chart, not the bio book. It tells the model *who exists
in this deployment*. Because names are role-shaped, the name itself
carries most of the disambiguating signal: when an agent is asked to
translate something, seeing `translator` in the index is enough to
delegate without a description fetch.

**Why this layer is not the same as the rejected `<peers>` block.**
Earlier in the design conversation we considered injecting full
`{name, description}` cards for every agent. That version was
heavyweight, branchy, and didn't match how humans build networks. The
name-only index is the *deferred-tools* analogue: every tool name is
always in context, full schemas are fetched on demand. Same pattern
here — every agent name is always in context, descriptions are fetched
on demand via Layer 4.

The index changes only when an agent is created, renamed, or deleted —
much less frequently than description edits — so it benefits maximally
from prompt caching.

### Layer 3 — The `<memory>` collaborator entries

Past delegations the agent has decided are worth carrying forward.
Stored under a new memory kind (§4 below), retrieved by the existing
contextual layer (`memory.md` §1.3) — top-K embedding similarity
against the session's opening message. Mid-session shifts use the
existing `recall` tool.

This is what "learned network" means in concrete terms. The first time
an agent successfully delegates to `designer`, it writes a memory
("for visual feedback, `designer` is the right call — responsive,
on-brief"). Future sessions retrieve that memory automatically when the
opening message is design-shaped.

**The contextual layer is the load mechanism.** No new loader, no
"refresh peers each turn." The collaborator network is just memory; it
loads exactly the way other memory does.

### Layer 4 — `search_agents(query)`

Semantic search over agents' descriptions. Used when name + memory
don't settle the question — usually a genuinely new kind of task, or
ambiguity between two plausibly-fitting names. Returns top-K
`{name, description}` cards. The caller is excluded.

After a successful delegation that started with `search_agents`, the
role prompt cues a `memory_write` so future sessions hit Layer 3
instead of paying the search cost again. The search is the path to a
new collaborator; the memory makes them a familiar one.

---

## 3. New `<core>` instruction: the delegation order

The `<core>` `<communication>` block (currently in `src/app.rs`) grows
a short paragraph teaching the four-layer order as an explicit rule.
Paraphrased:

> When an incoming message asks for work outside your role, do not
> attempt it. To delegate, choose a recipient in this order:
>
> 1. A name your role prompt names directly.
> 2. A name your memory records as the right collaborator for this
>    kind of task.
> 3. A name in `<agents>` whose role obviously fits the task.
> 4. The result of `search_agents(query)` when steps 1–3 don't yield
>    a clear answer.
>
> If `search_agents` returns nothing relevant, `send_message` the human
> asking which collaborator should own this — or whether to drop the
> request. Do not improvise the work yourself.
>
> When `search_agents` does yield a good collaborator, write a
> collaborator memory after the delegation succeeds so future-you skips
> the search.

Wording specifics will land with the implementation, but the load-bearing
parts are:

- **Strict ordering.** The model does not branch heuristically; it tries
  layers in sequence.
- **Layer 4 is the disambiguator, not the default.** A name in `<agents>`
  that obviously matches is preferred over searching.
- **The fourth-failure path is "ask the human", not "do it yourself".**
  This is the chain-of-command extension to the no-clear-recipient case.
  It is the active defence against drift; the role prompt's existing
  "What you do NOT memorise / What you do NOT do" firewall complements
  it.
- **The memory-write cue is explicit.** The role prompt rubrics already
  teach "write as you learn"; this generalises that to "write the
  collaborator after a discovery turn settles."

---

## 4. New memory kind: `Collaborator`

Today's memory kinds (`memory.md` §1.2) are `Self` / `Other` /
`Procedure` / `Open`. We add a fifth: **`Collaborator`** — beliefs
about other *agents* in the network, written from the agent's
delegation experience.

### Why a new kind rather than reusing `Other`

- `Other` already exists for "beliefs about specific peers or humans."
  We reserve it for facts discovered in production conversations — the
  human Sarah's sign-off habits, an external counterpart's tone, etc.
  Conflating it with internal-network memory makes operator audits
  noisier and the rubric harder to teach to the model.
- `Collaborator` memories are written on a different cue (a successful
  delegation), live a different lifecycle (they go stale when the
  operator renames or deletes an agent — Layer 2's `<agents>` list is
  the source of truth, the memory is the personal annotation), and
  benefit from being separable in the operator UI ("show me what this
  agent thinks about its peers").
- We have not released the product. Adding a kind now is free; carving
  one out of `Other` later is a migration.

### What `Collaborator` memories contain

The `<core>` cue for this kind (see §3 and §9.1) is:

- **Identity of the collaborator.** The agent name — always one of the
  names that exists in `<agents>`.
- **What you'd delegate to them.** The kind of task, in the agent's own
  words.
- **Why they were the right call.** What you observed in the
  collaboration that made you pick them.

Examples (rendered as the model writes them):

- *"`designer` is the right call for visual mockups; turned around a
  homepage in two passes, stays on brief."*
- *"`copywriter` handles long-form well but pushed back on landing-page
  CTAs — for ad copy, try `search_agents` first."*

### Lifecycle, state, librarian

- `Collaborator` follows the existing state machine (`tentative` →
  `held` → `validated`), exactly the same as the other kinds.
- The librarian's contradiction-detection logic operates over it
  unchanged (one collaborator memory contradicting another about the
  same agent triggers a resolution turn).
- A `Collaborator` memory whose target name no longer exists in the
  registry is **stale by construction** — it points at someone the
  deployment has deleted or renamed. The handling is "fail safely and
  let the librarian clean up over time"; see §9.3 for the full
  steady-state behaviour.

### Contextual retrieval

`Collaborator` joins the contextual-kind allowlist (currently
`Other` / `Procedure` / `Open`). The stable layer remains pinned + `Self`
only — collaborator knowledge is by nature contextual, surfaced only
when the session is about something that calls for delegation.

---

## 5. New field: `agents.description`

Operator-curated, **model-facing**, short — one sentence describing
what the agent is for. Distinct from `system_prompt`, which is the
operator-facing full role definition.

### Why a separate field

- `description` is for *being found*. `system_prompt` is for *being the
  agent*. They evolve for different reasons (tightening voice rules vs.
  re-pitching the role for discovery) and decoupling them keeps the
  search index stable across role iterations.
- `system_prompt` contains content that hurts embedding quality —
  negations ("you do NOT memorise visual choices"), examples, style
  guidance. Embedding it would yield blurry vectors. `description` is a
  clean positive statement of role.
- Operators retain editorial control over how their agents present
  themselves to the network.

### Required, non-empty

`description` is a **required field**. The column is `NOT NULL` with
no default; the smart constructor rejects empty / whitespace-only
strings; `POST /agents` and `PUT /agents/{id}` reject payloads without
it. There is no "empty description" path to handle.

This forces operator discipline: you cannot register an agent into the
network without saying what it's for. Every agent in `<agents>` is also
in `search_agents` results; the two surfaces never disagree about who
exists and who's discoverable.

The seeded default agent gets a description as part of its seed
constants alongside the existing default role prompt.

### Embedding

`description` is embedded (same provider as `recall`); the embedding
backs `search_agents`. Since `description` is required and non-empty,
every agent has an embedding and is reachable through search — no
degraded path, no operator UX where some agents are silently
invisible.

### Visibility

- `description` is returned in `search_agents` results.
- `description` is **not** rendered in the always-visible `<agents>`
  block — that block is name-only. Descriptions are a fetched view,
  consistent with the deferred-tools analogy.
- `description` is shown in the operator UI (agent CRUD) alongside
  `system_prompt`.

### Length

Short — the right ballpark is ~one sentence, well under the
`system_prompt` cap. The exact bound lands with implementation; the
design principle is that descriptions should be quick to read in a
top-K list, not paragraphs.

---

## 6. Naming: roles, not personas

Agent names are **role-shaped, snake_case, globally unique
(case-insensitive)**. Examples: `account_manager`, `brand_strategist`,
`copywriter`, `designer`, `project_coordinator`, `translator`.

This commits us to:

- **One agent per role per deployment.** The `agents.name` column gets a
  case-insensitive unique index. Two designers is operator error; if
  multi-instance roles ever become a real need, the answer is either a
  separate structured `role` column or a naming convention
  (`designer_brand` / `designer_web`) — not yet decided, not in scope.
- **The model sees names directly.** The earlier invariant that "the
  model never sees the name, only the resolved `system_prompt`"
  (`src/agents/types.rs` doc comment) is dropped. Names are now part of
  the model-facing surface — they appear in `<agents>`, they are the
  addressing key for `send_message`, they appear in `<memory>` when the
  agent writes about collaborators.
- **The human-facing UX uses the role name.** The Northstar pitch's
  personas (Riley/Sam/Jamie/Casey/Morgan) are dropped from the
  addressing layer; the human sees role names in transcripts. This is
  fine and arguably clearer — "the designer is asking for clarification"
  reads as well as "Casey is asking for clarification".

### Name uniqueness scope

Globally unique on `lower(name)`. Tenants are not yet a concept in
patom-rs; when they land, uniqueness becomes tenant-scoped in the same
migration that introduces the tenant boundary. Not a problem to solve
twice.

### Default agent

The seeded default agent keeps the name `assistant` — already
role-shaped, no rename required.

---

## 7. `send_message` and `search_agents`: addressing surface

### `send_message`

- The receiver shape becomes name-based for agents:
  `{kind: "agent", name: <role_name>}`. The id-based path is removed —
  there is no model-facing surface that produces ids any more, so
  carrying both shapes is dead weight (CLAUDE.md §1, no compat
  hacks pre-release).
- Human receiver is unchanged: `{kind: "human"}`.
- Resolution failure (unknown name, or a name that exists but the
  caller is forbidden from messaging in some future scoping model)
  returns an invalid-input error the model can read and react to.

### `search_agents`

- Single tool, single shape: `(query, limit?) -> [{name, description}]`.
- The caller is **excluded** from results — both for self-message-
  prevention parity with `send_message`, and because including
  guarantees a non-actionable result row.
- `limit` is bounded by a constant (`MAX_SEARCH_AGENT_RESULTS`); the
  exact value lands with implementation but should be roughly the same
  scale as `recall`.
- Results are not paginated. The top-K is the answer; if it doesn't
  contain the right collaborator, the answer is "ask the human"
  (per §3).

### What we are **not** adding

- **No `get_agent_card(name)` retrieval tool.** The model has enough to
  decide from `{name, description}`; the full role definition is
  internal to that agent and not part of the network's discovery
  surface.
- **No `list_agents` tool returning everything.** The `<agents>` block
  is the always-visible flat list; if the model wants more than a name,
  it queries with intent (`search_agents`). Bulk dumps are not a
  primitive.

---

## 8. The `<agents>` block — rendering rules

- **Position.** Sits between `<core>` and `<role>` in the assembled
  system prompt, mirroring the position of `<memory>`. Conceptually
  it's a structural fact about the deployment, not the role and not
  the agent's accumulated knowledge.
- **Content.** Comma-separated list of names, alphabetised. Caller
  excluded. No annotations, no descriptions, no ids.
- **Empty deployment.** If the caller is the only agent, the block is
  omitted entirely — no `<agents></agents>` envelope. The role prompt's
  named peers still apply (a solo agent has none, which is correct).
- **Cap.** A `MAX_AGENT_NAMES_INLINE` bound (per CLAUDE.md §5). Below
  the cap, the full list renders. Above the cap, the block degrades to
  a one-line notice ("N agents available; use `search_agents` to find
  one") — the same graceful-degradation pattern used elsewhere in the
  codebase. The exact cap lands with implementation; it should be
  generous enough to cover the realistic mid-market deployment without
  forcing operators to rely on search for routine hops.
- **Staleness.** Sourced from the agents store on system-prompt
  assembly. Refreshes within the existing agent-prompt cache TTL (60s
  today). Operators creating/renaming agents see the new list propagate
  within one TTL window — same liveness model as system-prompt edits.

---

## 9. Things to pay attention to

These are the cross-cutting concerns that don't sit neatly in any one
section but will bite if missed.

### 9.1 What lives in `<core>` vs. what lives in the role prompt

The four-layer model has two kinds of instructions, and they belong in
different places:

- **Generic, applies to every agent → `<core>`.** The delegation order
  itself (§3) and the cue to write a `Collaborator` memory after a
  successful delegation. These are mechanisms of the network, not
  facets of any one role. Every agent in every deployment behaves the
  same way here. Put them in `<core>` `<communication>` so operators
  don't have to re-paste them into every role prompt — same reasoning
  the existing `send_message` and "plain text is private" rules already
  use that block.
- **Role-specific → the role prompt.** The named procedural peers
  (Layer 1) — who you talk to as a matter of job description. The
  Northstar pitch's role prompts already do this ("you brief the
  writer and designer"); no rubric extension is needed for the
  collaborator memory-write rule, because that rule is in `<core>`.

The pitch's §3 prompts therefore need *no* change for collaborator
memory writes. They keep their existing rubric and procedural-peer
naming. The only `<core>` growth is the delegation-order paragraph and
the memory-write cue paired with it.

### 9.2 The deferred-tools analogy is the one to keep in mind

When in doubt about whether to inject something always-on or fetch it
on demand, the rule is:

- **Always-on** = identity-shaped, low-cardinality, cheap, stable. Names
  qualify.
- **Fetched** = descriptive, high-cardinality, expensive, volatile.
  Descriptions and full role prompts qualify.

This is the principle that distinguishes `<agents>` (always-on, names
only) from the rejected `<peers>` block (always-on, names + descriptions).

### 9.3 Memory contradicting registry

`Collaborator` memories carry agent *names*, not ids. If the operator
renames `designer` to `visual_designer`, every memory referencing
`designer` becomes stale. The handling is **the existing librarian
plus the runtime self-correction loop** — no new sweep behaviour is
introduced for this case.

Concretely:

- The next `send_message(name="designer")` fails with `unknown agent`.
  The model reads the error, re-derives via `search_agents`, delegates
  to the new name, and writes a fresh `Collaborator` memory.
- The stale row accrues no further validations and no further accesses
  (the model has stopped reaching for it). Score drops; the librarian's
  existing **decay** pass demotes stale `Validated` rows (`librarian.rs`
  §3), and the existing **eviction** pass (§4) removes the lowest-scored
  non-pinned rows when the agent hits `MAX_MEMORIES_PER_AGENT`. The
  stale memory is cleaned up on the same machinery that already cleans
  up every other unused row.
- The librarian's **contradiction detection** (§5) does *not* fire for
  the rename case, because its heuristic looks for opposing negation
  tokens and "use `designer`" vs "use `visual_designer`" carries no
  negation. That is fine; we do not need a contradiction event for
  this — the failed tool call is the contradiction event, and the
  resolution is the rediscovery.
- The librarian's **dedup** pass (§1) is a known small wart in this
  case: if the post-rediscovery memory ends up embedding-similar to the
  stale one above `DEDUP_SIMILARITY_THRESHOLD`, dedup keeps the *older*
  row by `created_at` tiebreak (`pick_dedup_loser`). The model then
  fails again on the next relevant turn, rediscovers, repeats. The
  threshold is high enough that texts mentioning different agent names
  are usually below it, so this fires rarely; when it does, the system
  oscillates but converges (eviction eventually drops one row). Not a
  blocker for v1; worth revisiting only if rename frequency turns out
  to be higher than expected.

This is also why we made memories key off names, not ids — names are
the model's mental model of who's around, and the failure mode of a
stale memory is a self-correcting tool error rather than a silent
delegation to the wrong place.

### 9.4 Exclusion rules — caller-excluded, consistently

Three surfaces exclude the caller:

- `<agents>` block — caller's name is not in the list.
- `search_agents` results — caller is filtered out before top-K.
- `send_message` — the existing self-message rejection stays unchanged.

Consistency matters: if `<agents>` listed the caller and `search_agents`
did not, the model would learn one rule from the index and another from
the tool, producing brittle behaviour at the seam. All three are
caller-excluded, full stop.

### 9.5 What the model never sees

For clarity, the operator-only surface (unchanged by this design):

- Agent UUIDs — the model addresses by name; ids never enter the
  prompt or the tool surface.
- `system_prompt` of other agents — internal to that agent. The only
  cross-agent disclosure is `description`, on demand.
- The agent's own `system_prompt` — they see the *rendered* result
  (which incorporates their role definition), not the row as it sits in
  the store. This was already true; just noting it for the new
  `description` field, which the agent also does not see for itself.

### 9.6 Migration is greenfield, no backfill

The product has not launched. There is no live data; the migration
adds `description` as `NOT NULL` directly with no default and no
backfill step. The down migration drops the column.

The only seeded row is the default agent, which gets its
description from the seed constants in the same place
`DEFAULT_AGENT_ROLE_PROMPT` already lives.

### 9.7 Operator UX implications (out of scope for this doc)

The web UI's agent CRUD pages will need a `description` field; the
`AgentMemoryPage` (already exists for memory inspection per the user's
project memory) gains a `Collaborator` kind in its filters. These are
mentioned for completeness — they belong to the implementation plan,
not this design.

---

## 10. Scope boundaries

### In scope

- New memory kind `Collaborator` and its rubric.
- New field `agents.description` and its embedding.
- New tool `search_agents`.
- `<agents>` name index block in the assembled system prompt.
- `<core>` `<communication>` paragraph teaching the delegation order.
- Name-based addressing in `send_message`; id path dropped.
- Case-insensitive global uniqueness on `agents.name`.
- The Northstar pitch's role prompts need **no** changes — the
  collaborator memory-write cue lives in `<core>` (§9.1).

### Out of scope

- **Per-tenant uniqueness scope** — tenants are not yet a concept.
- **A structured `role` column** — names carry role meaning; no second
  field until multi-instance roles become real.
- **Peer-graph enforcement** ("Riley cannot directly page Casey" as a
  hard rule). The role-prompt firewall is the v1 mitigation; structural
  enforcement is Phase 2.
- **Negative collaborator memories** ("X returned off-brief work"). Risk
  of unpredictable behaviour; revisit when a feedback mechanism for
  agents lands.
- **New librarian sweep behaviour for stale collaborator memories.**
  The existing decay + eviction passes handle this case adequately
  (§9.3); no new sweep is introduced.
- **`get_agent_card(name)` or similar deferred-fetch tools.** YAGNI;
  `description` in the search result is sufficient to decide.
- **Operator UI changes** beyond the agent CRUD field and memory-kind
  filter — separate web-side work.

---

## 11. Open question for the implementation plan

One question this design intentionally does not settle, because it
belongs to the implementation phase:

- **Cap values.** `MAX_AGENT_NAMES_INLINE`, `MAX_SEARCH_AGENT_RESULTS`,
  and `agents.description`'s byte cap are all named here but
  unvalued. The right numbers are best chosen against concrete token
  budgets and the realistic-deployment scale we expect (the pitch's
  ~5–30 agent range as the floor; mid-market deployments as the ceiling).
  These land with the implementation plan.

Everything else in this document is a commitment.
