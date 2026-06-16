# Per-person profiles + unified `search_colleague` (issue #183)

Status: **LOCKED 2026-06-16**. Substrate for the recruiter→HRM evolution.
Refs: [#183](https://github.com/tomkapa/patom/issues/183). Related: [[agent-channel-dm-addressing]], [[colleagues-identity-rework]], [[thread-chat-agent-refactor]].

---

## 1. Problem

Patom models agents as human coworkers, so an agent joining a thread should understand *who* it is working with the way a person would. Today it cannot. There are **three** distinct gaps, only two of which the issue names:

| Layer | Question an agent has | Today |
| --- | --- | --- |
| **L1 — who is in *this thread*** | who raised it, who @-tagged me, who else is here | **Missing.** `dm_counterpart` tracked but never surfaced (`threads/traits.rs:226`); no thread-creator or mention surfacing into the prompt. |
| **L2 — who *are* these people** | their role / expertise / preferences | **Missing.** `ColleagueRef = {id, kind, display_name}` (`colleagues/types.rs:138`); roster renders `- name — kind, id` (`colleagues/render.rs:77`). |
| **L3 — who in the *org* can do X** | semantic delegation, incl. people not in the thread | **Agent-only.** `search_agents` ranks `agents.description_embedding`, excludes humans (`agents/pg_store.rs:426`); humans have no embedding. |

The user's lived complaint ("an agent joins a conversation and doesn't know who tagged it, who's around, especially who raised the issue") is **L1**. The issue text is **L2 + L3**. This plan does all three, because L1's prompt block is the natural host for L2's per-person profiles.

### Why the issue's "reuse `collaborator` memory" lean is wrong

`memory_write(kind=collaborator, subject)` is **per-agent and private** — keyed by `agent_id` (`tools/system/memory/write.rs`). It can never be the *common board* the HRM vision needs (recruiter DMs a human → records their role → **every** agent can then find them). A common board must be **org-shared and singular**: one profile, one embedding per person. So we apply the field's standard **dual-memory split**:

- **Shared profile board** = "who they are" (org-visible, one per colleague) → feeds the prompt block *and* the human side of search.
- **Private `collaborator` memory** = "what *I* learned working with them" (unchanged) → an overlay on top.

This mirrors Letta/MemGPT memory blocks (always-in-context `human` block + searched archival), A2A agent cards (structured capability descriptor that is *discovered*), and CrewAI delegation (coworkers carry a **role**, not just a name).

---

## 2. Locked decisions

1. **Org-shared profile board**, not private memory and not columns on `users`. New table `colleague_profiles` keyed by `colleague_id` (already org-scoped → multi-org safe).
2. **Thread participant context (L1) is in scope** for #183, surfaced in the same `<participants>` block as the profiles (L2).
3. **Substrate only.** #183 ships: profile board + `profile_write` tool + unified `search_colleague` + `<participants>` block + the L1 plumbing it needs. The **recruiter→HRM behavior** (proactive DM-to-collect-profile, MCP-health inspection tool) is a **follow-on** that consumes this substrate.
4. **One search path, two embedding sources.** `search_colleague` UNIONs `agents.description_embedding` ∪ `colleague_profiles.embedding`, both `vector(1536)` from the same provider seam, into one ranked result. No `search_human` / `search_agent` duplication; `search_agents` is renamed, not supplemented.
5. **No agent-data duplication.** Agents keep `agents.description_embedding` as their card. `colleague_profiles` rows are minted for **humans** now; the table is keyed by `colleague_id` so it can hold agent override rows later without a schema change, but we do not populate them in #183.

---

## 3. Design

### 3.1 Data model — `colleague_profiles`

New table (migration, paired up/down):

```
colleague_profiles
  colleague_id          UUID PRIMARY KEY REFERENCES colleagues(id) ON DELETE CASCADE
  org_id                UUID NOT NULL REFERENCES organizations(id)   -- denormalized for RLS + scoping
  role                  TEXT NULL          -- "Product Manager", bounded
  expertise             TEXT NULL          -- free text, bounded
  preferences           TEXT NULL          -- "call me Pa; async-first", bounded
  profile_text          TEXT NOT NULL      -- composed embedding source (role+expertise+preferences), bounded
  embedding             vector(1536) NULL  -- NULL until first write; search skips NULLs (degraded layer, like agents)
  updated_by_colleague  UUID NULL REFERENCES colleagues(id) ON DELETE SET NULL  -- provenance: HRM vs self vs peer
  created_at            TIMESTAMPTZ NOT NULL
  updated_at            TIMESTAMPTZ NOT NULL
```

- **RLS**: org-membership policy like other tenant tables; the prompt-path read may run **privileged** the way the roster read does (`colleagues/pg_store.rs:73`), since it joins `users`/`colleagues` that are REVOKEd from `patom_app`.
- **Embedding parity**: same model/dimensions as agents (`provider/embedding.rs:25`, 1536) so a cross-source UNION ranks by comparable cosine distance.
- Structured `role`/`expertise`/`preferences` are kept *and* flattened into `profile_text`; the structured fields render cleanly in the block, the flattened text is what we embed.

### 3.2 Newtypes (§1 Types encode invariants)

In `colleagues/profile/types.rs` (new), each `TryFrom<&str>` enforcing its cap, all readers only:

- `Role`, `Expertise`, `Preferences`, `ProfileText` — bounded strings (`ParseError::TooLong`).
- `ColleagueProfile { colleague_id, role: Option<Role>, expertise: Option<Expertise>, preferences: Option<Preferences>, updated_by: Option<ColleagueId> }`.
- `ProfileError` (thiserror) for the module boundary (§12), `From<sqlx::Error>`, `From<EmbeddingError>`.
- `profile_text` is derived, not user-set: a private `compose_profile_text(&ColleagueProfile) -> ProfileText` joins the present fields.

### 3.3 `ProfileStore` trait + Pg impl (§9, §10, §11)

`colleagues/profile/store.rs`:

```
trait ProfileStore {
    async fn upsert(&self, p: &ColleagueProfile, embedding: &[f32]) -> Result<(), ProfileError>;
    async fn get(&self, id: ColleagueId) -> Result<Option<ColleagueProfile>, ProfileError>;
    async fn get_many(&self, ids: &[ColleagueId]) -> Result<HashMap<ColleagueId, ColleagueProfile>, ProfileError>; // <participants> block
}
```

- Embedding computed **before** the upsert tx (abort on embedding failure — same discipline as agents at `agents/pg_store.rs:96`), via `embed_one`.
- All queries: bound parameters only (§10), `LEFT(col, N)` / app-side caps on TEXT reads (§5), `tokio::time::timeout` on every await (§5), batch cap `MAX_PROFILE_FETCH` on `get_many`.
- `#[sqlx::test]` integration tests against real Postgres (§3).

### 3.4 `profile_write` system tool

New `tools/system/profile_write.rs` — distinct from private `memory_write`:

- Input: `{ subject: ColleagueId, role?, expertise?, preferences? }`. Validates each via `TryFrom`. `subject` must resolve to a colleague in the caller's org.
- Effect: upsert the **org-shared** board row + (re)embed `profile_text`.
- Authority (v1): any agent in the org may write (org-scoped, RLS-guarded) — pragmatic for the pre-HRM recruiter to populate the board. Tighten to HRM-only later if abuse shows up (noted §7).
- Tool description steers usage: "record a colleague's durable role/expertise/preferences on the shared board so the whole org can find them; use `memory_write` for your own private notes."

### 3.5 `search_colleague` (rename + widen `search_agents`)

- Rename the tool `search_agents` → `search_colleague` (`tools/system/search_agents.rs`). One path; no second tool (acceptance criterion).
- Store: replace `search_by_description` with a UNION query returning `(colleague_id, kind, name, snippet, distance)`:
  - agents: `agents.description_embedding <=> $q` (join `colleagues` to get `colleague_id`/name), `org_id` scoped, caller excluded.
  - humans: `colleague_profiles.embedding <=> $q WHERE embedding IS NOT NULL`, same org.
  - `ORDER BY distance ASC LIMIT $k` over the union.
- Output widens to `{ matches: [{ kind, id, name, snippet }] }` so the agent gets a card it can act on via `send_message{to:{...}}` ([[agent-channel-dm-addressing]] already lets it pull a non-thread colleague into the room).
- **Callers to update**: every recruiter prompt that names `search_agents` — `prompts/en.toml:11` *and all other language registries* (recruiter seed reads per-language, `app.rs:601`). Checklist item.
- Humans with no profile yet have `embedding IS NULL` → invisible to search until profiled. This is the precise hook the HRM follow-on closes (DM → profile → discoverable).

### 3.6 `<participants>` block (L1 + L2)

New render fn `colleagues/render.rs::render_participants_block` and a new assembly step in `memory/agent.rs::system_prompt_for_thread` (`agent.rs:196`).

- **Inputs**: the thread's *active participant* colleague-ids + their thread roles + their profiles.
- **Active participants** resolved from: (a) thread creator/raiser, (b) distinct senders in the feed (`threads/traits.rs:287` already maps senders), (c) the colleague who addressed/@-tagged the agent this turn. Deduplicated, capped at `MAX_PARTICIPANTS_INLINE`.
- **Profiles** via `ProfileStore::get_many` for humans; `agents.description` for agent participants. One snippet each, length-capped.
- **Rendered shape** (illustrative):

```
<participants>
People in this conversation. The first message here was raised by Pa.
- Pa (human) — Product Manager; owns billing; prefers async — raised this thread, tagged you
- Mina (human) — Designer; ships fast, skips tests
- Scout (agent) — research specialist; web + retrieval
</participants>
```

- **Placement & caching (§prompt-cache rationale, `agent.rs:14`)**: `<colleagues>` sits in the per-org stable prefix. `<participants>` is **per-thread, per-turn** (varies by who's talking / who tagged me), so it must be emitted **after** the stable prefix — adjacent to `<memory>` in the per-turn tail — to avoid busting the cache prefix.
- **Degradation**: profile/thread-role lookups are enrichment, not load-bearing — any failure degrades to name+kind (or empty block) and logs, never fails the turn (same posture as the roster block at `agent.rs:184`).

### 3.7 L1 plumbing (thread participant context)

Minimal additions so the block has data:

- **Thread creator/raiser**: surface the creating colleague on the thread record (extend the thread read in `threads/`); today it is not exposed to the prompt path.
- **Addresser / @-tag of this turn**: the triggering message's sender + whether it addressed the viewer — thread through the turn-build path into `system_prompt_for_thread` (the addressing layer already routes mentions for delivery; we now also *report* it to the agent).
- No new mention-storage subsystem in #183 — derive "tagged you" from the turn's triggering message, not a historical mention index (keep scope bounded; full mention history is a later concern).

---

## 4. HRM follow-on (out of scope, sketched so the substrate fits)

The recruiter is already a seeded per-org agent that scopes roles, calls `search_agents`/`search_tools`, and wires MCP via `request_user_wire_mcp` (`app.rs:601`, `prompts/en.toml:11`). The follow-on turns it into an HRM **on top of this substrate**:

- Reshape the recruiter prompt: proactively DM a human for role/expertise → call `profile_write` to populate the board → answer "who can do X?" via `search_colleague`.
- New `mcp_health` inspection tool: `mcp_servers.connection_status` / `last_error` / `last_seen_at` already exist (`mcp/types.rs:477`) but no agent-facing tool surfaces them; add one so the HRM can spot a broken wiring and nudge the human.
- Optional self-service web route for a human to edit their own board entry (`updated_by_colleague` already records provenance).

None of this requires schema changes beyond #183.

---

## 5. Limits / constants (§5, in `colleagues/profile/limits.rs`)

- `MAX_ROLE`, `MAX_EXPERTISE`, `MAX_PREFERENCES`, `MAX_PROFILE_TEXT` — string caps (embedding-input bound).
- `MAX_PARTICIPANTS_INLINE` — cap on the `<participants>` list (mirrors `MAX_ROSTER_INLINE`).
- `PROFILE_SNIPPET_LEN` — per-person snippet length in the block.
- `MAX_PROFILE_FETCH` — `get_many` batch cap.
- `SEARCH_COLLEAGUE_K` — reuse the existing search limit (1–8).
- Each constant doc-commented with *why this number*. Saturation metrics: `patom.participants.block.size`, search result counts.

---

## 6. TDD staging (§3 — failing test first, gates green per stage)

0. **Migration** `colleague_profiles` (+ pgvector column, RLS policy, indexes) with tested reversible down.
1. **Newtypes + `ProfileError`** — `TryFrom` bound tests (red→green).
2. **`ProfileStore` + Pg impl** — `#[sqlx::test]` for upsert/get/get_many, embedding-failure-aborts-upsert, org scoping.
3. **`profile_write` tool** — validation, org-scope rejection, board upsert + re-embed.
4. **`search_colleague`** — rename; UNION store query; mixed agent+human ranked results; NULL-embedding humans excluded; caller excluded; org-scoped. Update all language prompts.
5. **L1 plumbing** — thread creator + turn addresser surfaced into the prompt path.
6. **`<participants>` block** — render tests (profiles + thread-role annotations, cap, degradation) + assembly placement test (after stable prefix).
7. **e2e** for the changed surface + full gate sweep: `fmt`, `clippy -D warnings`, `check`, `test`, coverage (80% overall; 100% on the bounded profile-text composer + search-union mapper), `cargo deny`/`audit`.

Suggested PR slices: **(0–4)** board + write + search, then **(5–6)** participants block + L1, to keep each PR reviewable.

---

## 7. Risks / open considerations

- **Write authority**: org-wide `profile_write` is permissive. Acceptable for launch (small, friendly user base — [[product-in-production-migrations-need-backfill]]); revisit HRM-only gating with the follow-on.
- **Cross-source ranking**: agent descriptions are rich, human profiles sparse → agents may dominate `search_colleague`. Mitigate by returning both kinds and letting the prompt steer ("search for a person vs a specialist"); consider a kind filter arg if it skews in practice.
- **Cold board**: humans are unsearchable until profiled. Intended — it's the HRM's job to fill the board. Until then, humans remain discoverable via the `<participants>`/`<colleagues>` blocks.
- **Prompt-cache discipline**: the `<participants>` block must stay out of the stable prefix; a misplacement silently tanks cache hit-rate. Covered by the assembly placement test (stage 6).
- **Migration discipline (§13)**: live prod data; standard reversible up/down, no edits post-merge.
