# ADR-0003 — Per-agent private memory with idleness-driven reflection

- **Status:** Accepted
- **Date:** 2026-05-08
- **Deciders:** core

## Context

The product thesis is that agents behave like real employees — they accumulate knowledge, refine preferences, respect operator-set core values, and self-evolve over time without drifting away from their pinned identity. Three options for "memory":

1. **No persistent memory.** Every session starts cold. Doesn't fit the thesis.
2. **One shared memory store across all agents.** The account-manager's beliefs about the client leak into the copywriter's voice rules. Drift is catastrophic and unfixable.
3. **Per-agent private memory.** Each agent owns its journal; cross-agent reads are not allowed.

The second axis is *when* memory updates:

1. **In-turn writes only.** The agent writes memory inside a normal turn. Implicit-signal capture during the conversation pollutes the journal with low-quality writes; explicit-signal capture works but loses everything the model noticed without being asked.
2. **End-of-session writes.** Sessions are never assumed to "end" in Patom — a human may resume tomorrow. There is no natural close to trigger a write.
3. **Idleness-driven reflection.** When a conversation has been idle long enough, a background turn consolidates what was learned.

## Decision

**Per-agent memory is private and persistent. Memory mutations come from three contexts only — normal turn (explicit conversational request), reflection turn (idleness-driven), or librarian-resolution turn (focused contradiction handling).**

- Each agent has a journal (`memory_events`) and a materialised current state (`agent_memories`).
- Memory is composed into the system prompt in two layers: a stable layer (pinned + Identity-kind) and a contextual layer (top-K embedding retrieval against the session opener). Both are assembled at session start and **frozen** for the session's life so the cached prefix stays stable across turns.
- Reflection runs when `(agent, session)` has been idle past `REFLECTION_IDLE_TIMEOUT_SECS` and there are turns past the latest `reflection_checkpoints` row. The scheduler polls and enqueues a `kind = reflection` prompt request.
- Reflection turns see the conversation transcript and the same memory layers a normal turn would, but **not** the prior reflection's reasoning — the structural anti-self-reinforcement guard.
- Reflection runs off-DAG: it doesn't consume the human turn's DAG budget.

## Consequences

**What becomes easy:**

- Each agent's beliefs are auditable in isolation. The account-manager's client memory and the copywriter's voice rules live in different rows.
- Memory consolidates without explicit user action — the system catches what the agent learned during a busy conversation once the burst ends.
- A session that resumes tomorrow gets a fresh reflection only on the new turns (idempotent via the checkpoint).
- Pinning is the operator override — invariants the agent cannot edit.

**What becomes hard:**

- A reflection that fires while a user is composing a follow-up could miss those new turns. The checkpoint ensures the next reflection catches them; we accept a one-cycle lag rather than blocking writes.
- Reflection without a visible audience is unnatural for the model. The reflection-specific `<core>` prompt has to be precise — output structured tool calls only, no prose. Drift here is dampened by a hard cap on mutations per reflection.
- Cross-session re-emergence of the same belief is *not* a validation signal (the prior memory is typically loaded into the new session's stable layer and re-stated, so re-emission is self-citation with extra steps). This required carving out "independent signal" carefully (see [ADR-0009](./0009-typed-memory-states-not-numeric-confidence.md)).

**What we live with:**

- A small additional Postgres load from per-agent reflection scans. Bounded by the polling cadence and the idle-timeout gate — the scheduler picks up only sessions that actually have new turns.
- Reflection latency: a conversation that ends at T won't be consolidated until T + idle timeout. We've decided that's fine — memory consolidation in real organisations happens after the meeting, not during it.

## Alternatives considered

- **Shared memory store across all agents.** Drift is catastrophic and unfixable. Rejected.
- **In-turn-only memory writes.** Loses implicit-signal capture; over-fits to whatever the conversation asks for explicitly. Rejected.
- **End-of-session writes.** Sessions never end in Patom — there is no natural trigger. Rejected.
- **Reflection sees its own previous reasoning.** Self-reinforcement loop: the model echoes yesterday's conclusion, the librarian validates the echo, the belief calcifies regardless of whether it was right. Rejected — reflection sees user-facing conversation only.
