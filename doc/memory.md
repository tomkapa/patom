# Agent Memory

## 1. Feature

### 1.1 Purpose

Each agent in Patom has its own private memory that persists across sessions. Memory is the agent's distilled understanding of its own role, the peers and humans it works with, and the procedures it has learned — extracted from past conversations and held in a bounded, typed, journaled store.

Memory is the *summary* of experience, not the experience itself. Raw conversation logs continue to live in the messages table; memory holds only what the agent has decided is worth carrying forward.

The design serves one product goal: agents that behave like real employees — they accumulate knowledge, refine preferences, follow operator-set core values, and self-evolve over time without drifting away from their pinned identity.

### 1.2 What a memory is

A single memory row carries:

- **content** — one or two sentences. The memory itself.
- **kind** — exactly one of:
  - *Self* — identity, style, preferences ("I default to terse replies").
  - *Other* — beliefs about specific peers or humans ("translator is fast on European languages").
  - *Procedure* — learned how-tos ("for ambiguous deadlines, propose a date and ask to confirm").
  - *Open* — known unknowns ("I don't have a confident play for vague translation targets").
- **state** — exactly one of:
  - *core* — operator-pinned, immutable to the agent.
  - *validated* — confirmed by independent signals.
  - *held* — default accepted.
  - *tentative* — newly written, unverified.
- **pinned** — operator flag. Pinned memories cannot be updated or forgotten by the agent.
- **provenance** — the turn that produced this memory.
- **embedding** — vector for similarity retrieval.
- **timestamps** — `created_at`, `last_validated_at`, `last_accessed_at`.
- **access_count** — read counter. Frequently-read memories survive decay longer.

The hidden underlying confidence is a number; the agent only ever sees the qualitative state. This avoids calibration drift — agents reason about "tentative vs validated" reliably; they reason about "0.42 vs 0.61" poorly.

What memory does NOT store: identity facts that exist in the agent registry (peer ids, capabilities, role descriptions), transient session state, or raw episodic content. Facts come from the registry, episodic content from the message log, beliefs and learnings from memory.

### 1.3 How memory enters the system prompt

Memory is composed into the system prompt after the role block, in two layers. Both layers are assembled at session start and frozen for the session's lifetime — within a session, memory is read-only, which keeps the cached prefix stable across turns.

System prompt composition for any turn:

```
core              (binary constant, communication protocol — same for every agent)
role              (per-agent column on the agents record — operator-set identity)
memory:
  stable layer    (pinned + Self-kind, token-capped)
  contextual      (top-K embedding-retrieved Other/Procedure/Open, against
                   the session's opening context)
```

The user prompt is unchanged — it remains whatever the human or peer agent sent. Memory does not enter the user prompt as ambient data.

#### Render shape

Memory renders as a kind-grouped bulleted list with short handles and state tags:

```
## Memory

### Self
- [M-12, validated] You default to terse replies unless asked otherwise.
- [M-18, held] You ask one focused clarifying question rather than several.

### Other
- [M-31, held] translator is fast on European languages; ask for context with idioms.
- [M-44, tentative] marketing-lead asks vague questions — ask back before delegating.

### Procedure
- [M-07, validated] For ambiguous deadlines, propose a concrete date and ask to confirm.

### Open
- [M-52, tentative] You don't have a confident play for handling vague translation targets.
```

Three properties this gets right:

- The state tag is visible to the agent so it can adjust trust without seeing a number.
- Short handles (M-NN) — not raw UUIDs — are stable for the session and cheap to dereference. The agent uses these in tool calls.
- Kind-grouped structure teaches the agent how to relate to each kind by observation.

### 1.4 How memory is read mid-session

If the contextual layer missed something the agent needs, it calls the `recall` tool:

```
recall(query: string, kind?: MemoryKind, limit?: u8)
```

The tool runs an embedding query against the agent's memory store and returns matching rows as a tool result. The result lands in the conversation as a normal tool-call / tool-result pair. Per-turn caps prevent spam.

`recall` is for nuance, not load-bearing knowledge. Anything the agent must not lose belongs in the stable layer (pinned + Self).

### 1.5 How memory is written

The agent has three memory mutation tools:

```
memory_write(kind, content)
memory_update(handle, content)
memory_forget(handle)
```

These tools are registered once and available in every turn the agent runs. There is no special "memory mode" or per-context tool surface. What varies between contexts is the *job* the agent is doing on that turn:

- **Normal turn** — the agent uses memory tools only when the conversation explicitly asks for it. "Remember I prefer tabs over spaces" → `memory_write`. "Your memory M-12 is wrong, the deadline is Tuesday" → `memory_update`. The agent does NOT write memory based on weak or implicit signals during a normal turn — implicit-signal capture is reflection's job, not the current turn's.
- **Reflection turn** — autonomous self-curation. The agent reviews the conversation since the last checkpoint and decides what to write, update, or forget. Hard cap on mutations per reflection.
- **Librarian-resolution turn** — focused on a single contradiction pair detected by the librarian. The agent decides which memory to keep, update, or forget.

All three contexts call the same tools, write through the same journal-append-plus-upsert path, and produce the same shape of mutation. Only the prompt's framing of the agent's job differs.

### 1.6 Reflection turns — autonomous self-curation

A reflection turn is what the agent does to consolidate after a conversation. It runs in the background, has no audience, and produces only memory mutations.

#### When reflection runs

A scheduled sweep finds sessions where:

- the time since the last turn exceeds a configurable idle timeout, AND
- there are turns past the latest reflection checkpoint for that (agent, session).

Those sessions qualify for reflection. Sessions are not assumed to ever "end" — a human may resume tomorrow. Reflection runs whenever a session has been idle long enough and has unprocessed turns.

#### Checkpointing

Each (agent, session) carries a `reflection_checkpoint` row that records the last turn id processed by the most recent reflection. The next reflection processes only turns after that point. When reflection completes, the checkpoint advances atomically with the journal writes.

This makes reflection idempotent across session resumption. A session that gets reflected on Monday and resumes Wednesday will produce a second reflection on Wednesday's idle timeout, processing only the Wednesday turns.

#### Reflection's inputs

A reflection turn sees:

- The agent's role block (identity preserved across normal and reflection turns — the translator reflecting is still the translator).
- A reflection-specific core prompt (see §1.6 composition below) that defines the reflection protocol, output format, and write quotas.
- The new turns since the last checkpoint.
- The same memory layers a normal turn would see, retrieved against the new turns rather than the session opener — stable layer + contextual top-K. This is bounded by the same token cap as a normal turn; reflection does NOT see the agent's full memory state.

Reflection does NOT see the prior reflection turn's reasoning or tool calls. The previous reflection's *outputs* are already merged into the agent's current memory state and visible there; its *reasoning* stays private. This is the structural anti-self-reinforcement guard.

#### System prompt composition for a reflection turn

```
reflection_core   (binary constant — replaces the normal core for reflection)
role              (same per-agent column as normal — identity preserved)
memory:
  stable layer    (same assembly as normal turns)
  contextual      (top-K against the new turns being reflected on)
```

#### Output

Structured tool calls only — `memory_write`, `memory_update`, `memory_forget`. No prose, no audience. A hard cap limits total mutations per reflection so dreams cannot produce 30 writes a night and bloat the journal with noise.

### 1.7 Lifecycle — how memories age and change state

A new memory enters the system as `tentative`.

Promotion:

- *tentative → held* — two paths. *Active*: an independent signal confirms the memory (see the validation clock below). *Passive*: the memory survives the configurable maturation window (`MATURATION_WINDOW`, currently 7 days) without being merged out, forgotten, or entangled in an unresolved contradiction. Passive maturation runs in the librarian's mechanical sweep and is **state-only** — `last_validated_at` is not advanced.
- *held → validated* — an *independent* signal confirms the memory: an external confirmation event, or an operator endorsement. Passive time-survival does not reach `validated`; that rung is reserved for genuine independent evidence.

Demotion:

- *validated → held* — extended non-access (decay).
- *any non-pinned → forget* — confidence floor crossed plus age threshold plus low access count.

Pinned memories are exempt from every demotion path. A signal that contradicts a pinned memory is journaled for operator review rather than changing the state.

The validation clock — `last_validated_at` — advances ONLY on independent signals:

- External confirmation (web search, peer agent, human, or the user affirming in the current turn — captured by the agent via `memory_validate` with the supporting quote as evidence).
- Operator endorsement via `manager_note`.

Cross-session re-emergence is **not** a validation signal, even though the same content appearing in a different session is suggestive. The reason: every session loads the agent's stable + contextual memory layers, so a memory minted in session S1 is typically already in session S2's system prompt by the time S2's reflection might re-emit it. The re-emergence is then self-citation with extra steps, not independent re-derivation. The librarian still dedups same-content rows (the loser is forgotten), but the survivor's validation clock does not move.

Mere reading into context does NOT advance validation. A memory that's read constantly but never re-validated lives (because of the access counter) but does not promote (because validation requires independent evidence).

### 1.8 Librarian — mechanical maintenance and contradiction resolution

The librarian runs on a schedule, per agent. It has two phases:

#### Mechanical sweep (no LLM)

Pure SQL plus embedding operations:

- *Dedup* — pairs with cosine similarity above a threshold are merged, keeping the higher-state and older-provenance copy. The loser is forgotten; the survivor's validation clock is not touched (cross-session re-emergence is not an independent signal — see §1.7).
- *Maturation* — non-pinned `tentative` rows older than `MATURATION_WINDOW` (currently 7 days) that are not referenced by any unresolved `contradiction_events` row promote to `held`. State only — `last_validated_at` does not move. This is the passive path that lets internal-only beliefs (preferences, identity traits the agent cannot externally verify) leave `tentative` before they decay out under quota.
- *Decay* — `last_validated_at` thresholds drop confidence; state demotes when buckets cross.
- *Eviction* — when an agent is over its memory quota, lowest-score non-pinned memories are forgotten (forget event in the journal).
- *Contradiction detection* — pairs with high embedding similarity but textually opposed signals get written as `contradiction_events` rows. Detection only — no resolution. Session-blind by design: if the agent is internally inconsistent across two writes (whatever their origin), the resolution turn is the right place to clean it up.

#### Resolution turn (LLM, focused)

If the mechanical sweep produced unresolved contradiction events for an agent, a focused turn runs once per pair:

- The agent's role block (identity preserved — resolution speaks with the agent's voice).
- A resolution-specific core prompt — bounded, single-job: "given memory A and memory B (which contradict), keep, update, or forget."
- Inputs: the two memories and their provenance arrive as the resolution turn's **user prompt body** (handles `M-1` and `M-2`, fixed by `contradiction_events` column order). The agent's standard `<memory>` block continues to render the stable + contextual layers at `M-3..` so related memories can inform the decision; the pair-side rows are deduped from the layered text to avoid two renderings of the same memory. No conversation history from the parent session — the resolution session is its own (Agent, System) pair with `parent_session_id = NULL`.
- Output: structured tool calls (`memory_update` or `memory_forget` against `M-1`/`M-2`) or "no action — both correct in different contexts," which marks the contradiction event resolved without a write. The agent may use `recall`, `web_search`, `web_fetch`, or `send_message` (to ask a human) during the turn before committing.

This isolation is deliberate. The agent's job for the resolution turn is solving one contradiction; pulling in conversation history or other open contradictions would dilute the focus. Memory state is retained — surfacing related memories is more often help than noise, and the pair is the highlighted subject via the user prompt body.

### 1.9 Operator authority

The operator (today: the single human responsible for the deployment) has read, pin, and revert authority on the journal:

- `manager_note(agent_id, content, kind, pinned)` — direct memory write, attributed to the operator. Notes can be written at `validated` or pinned at `core` immediately, since operator endorsement is one of the validation signals.
- *Pin / unpin* — toggles the pinned flag on a memory, raising or lowering its protection.
- *Revert* — appending an inverse journal event undoes any past mutation. The materialized view recomputes from the journal.

There is no inline approval — operator audit happens asynchronously. This preserves agent autonomy. The model is "agent acts, operator audits later," like a manager reviewing an employee's work.

### 1.10 Drift defenses

The structural separations above kill the catastrophic drift modes. Smaller defenses handle everyday belief noise:

- *Facts vs beliefs* — registry holds facts (peer ids, capabilities, role descriptions); memory holds beliefs only. Catastrophic facts cannot drift because they are not stored as memory.
- *Memory mutates only on explicit job* — normal turns mutate only on explicit conversational request; otherwise reflection or resolution does it. No implicit mid-turn writes.
- *Nuance by default* — every agent-written memory starts `tentative`. Promotion requires independent validation or operator pinning.
- *Operator pin* — invariants the agent cannot edit.
- *Decay* — `last_validated_at` ages mechanically, no LLM cost.
- *Quarantine* — `tentative` cannot promote until a librarian sweep confirms it survives dedup and contradiction.
- *Anti-self-reinforcement* — reflection sees user-facing conversation, not its own previous reasoning.
- *Independent-signal-only validation* — neither self-citation in a dream nor cross-session re-emergence (which is typically just the prior memory being loaded into the new session's stable layer and re-stated) advances the validation clock. Only external confirmation and operator endorsement do.
- *Passive maturation only reaches `held`* — long-lived tentative memories promote to `held` without independent evidence, but `validated` stays reserved for real signal. The two-rung ladder keeps "I think this is true" and "this has been verified" distinguishable.
- *Journal as truth* — every mutation is replayable and revertable.
- *Per-agent quota* — hard cap on memory rows; eviction forces real curation rather than hoarding.

### 1.11 Boundaries

- Memory is per-agent and private. No cross-agent reads.
- New agents start with empty memory. Cloning is a future operator action, not a default.
- Memory does not duplicate registry data or session messages — it summarizes only.
