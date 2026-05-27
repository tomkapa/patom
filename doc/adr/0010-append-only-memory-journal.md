# ADR-0010 — Append-only journal for memory + replayable revert

- **Status:** Accepted
- **Date:** 2026-05-10
- **Deciders:** core

## Context

Memory is the agent's accumulated belief state. Mutations come from three contexts (normal turn, reflection, librarian resolution), each capable of writing, updating, or forgetting. Two storage shapes:

1. **In-place mutations on `agent_memories`.** Each write does an UPSERT; updates overwrite; forgets delete. Simple and small.
2. **Append-only journal (`memory_events`) + materialised current state (`agent_memories`).** Every mutation is an event; the current state is derived from replaying the journal.

In-place loses history. If a reflection turn forgets something it shouldn't have, we have no record of what was there. Operator audit ("show me everything this agent has ever believed about Sarah") is impossible.

We also need a revert primitive — an operator should be able to say "undo that mutation" without a deletion that can't itself be undone.

## Decision

**`memory_events` is the append-only journal. `agent_memories` is the materialised current state, derived from the journal.**

- Every mutation (`memory_write`, `memory_update`, `memory_forget`, `memory_validate`, state transitions from the librarian, operator pins/unpins) writes a `memory_events` row with the turn that produced it as provenance.
- `agent_memories` is the "current state" view — fast to read, replaced when the journal advances.
- **Revert is an inverse event.** An undo is "append the inverse mutation" — never a row deletion. The materialised view recomputes from the journal.
- The journal is the source of truth. The materialised view is rebuildable from it.

## Consequences

**What becomes easy:**

- Audit. Every belief mutation has a provenance — which turn produced it, when, in which context (normal/reflection/resolution), and at the operator's request or the agent's.
- Revert. An operator can undo any past mutation by appending the inverse, regardless of how long ago. The materialised view rebuilds; nothing is "permanently lost."
- Time-travel debugging. We can replay the journal to any point and see what the agent "knew" at that moment.
- Reflection idempotency. The journal carries the turn id; replaying does not double-apply.

**What becomes hard:**

- Two writes per mutation (journal + materialised view) instead of one. The cost is small per write and bounded by `MAX_MEMORY_MUTATIONS_PER_TURN`.
- The materialised view needs invariants — it must agree with the journal. A reconciliation pass (offline) is a follow-up item if drift becomes a concern; not implemented today because the write path is the only mutator and it always advances both atomically.
- Schema is bigger: journal columns include the full mutation shape, not just the delta. Acceptable given storage costs.

**What we live with:**

- The journal grows unboundedly. Retention/archival policy is a separate decision (not yet binding). For mid-market deployments at projected scale, the journal stays sub-gigabyte for years.
- A bug that produces a malformed event poisons the replay. The fix is to fix the bug and replay; a forensic edit of the journal would defeat the audit invariant and is forbidden — we will not provide a tool to do it.

## Alternatives considered

- **In-place mutations only.** Loses history; no revert. Rejected.
- **Snapshots at intervals + a small delta log.** More machinery for marginal benefit; replay through the full journal is fast enough at our scale. Rejected.
- **Soft-delete on `agent_memories` (a `deleted_at` column).** Hides the "forget" but doesn't record updates or state transitions. Half a solution. Rejected.
- **Event-sourcing framework (e.g. `eventstore`-style external DB).** Operational complexity for one subsystem; the rest of the system would still be Postgres. Rejected — a journal table in the same database is the right granularity.
