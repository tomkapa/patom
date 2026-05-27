# ADR-0009 — Typed memory states, not numeric confidence

- **Status:** Accepted
- **Date:** 2026-05-10
- **Deciders:** core

## Context

A memory carries some notion of "how strongly do we believe this?" Two representations:

1. **Numeric confidence.** A `f32` in `[0.0, 1.0]` (or a log-odds, or a Bayesian posterior). Easy to update arithmetically; hard for an LLM to reason about consistently.
2. **Typed states.** A finite ladder — `tentative`, `held`, `validated`, `core` — with explicit transitions.

Empirically, LLMs reason poorly about absolute numeric confidence ("0.42 vs 0.61" rarely produces stable behaviour across turns). They reason much more reliably about qualitative buckets ("tentative vs validated"). And the operator UI is clearer when the audit view says "this memory is `validated` because Sarah confirmed it on 2026-05-14" rather than "this memory is 0.78."

We still want a numeric signal internally — decay, eviction, dedup all benefit from a continuous score. But that score should not be the agent's mental model.

## Decision

**The agent sees a typed state ladder. A hidden numeric confidence drives mechanical demotion and eviction.**

```text
Tentative ──active validation──▶ Held ──independent signal──▶ Validated
    │                              │                              │
    │ passive maturation           │ operator pin                  │ extended
    │ (MATURATION_WINDOW)          ▼                              │ non-access
    └──────────────────────────▶  Core ◀────────operator pin──────┘
                                                                   │
                                                                   ▼
                                                                  Held (decay)

Any non-pinned: confidence floor + age threshold + low access → forgotten
```

- **Tentative** — newly written, unverified. The default for any agent-written memory.
- **Held** — default accepted. Reached either by active validation (one independent signal) or by passive maturation (survives `MATURATION_WINDOW` without contradiction or dedup-loss).
- **Validated** — confirmed by independent signal. Passive time-survival does *not* reach Validated.
- **Core** — operator-pinned. Immutable to the agent; exempt from every demotion path. A contradicting signal is journaled for operator review, not auto-applied.

Independent signals: operator endorsement (`manager_note`), external confirmation (web search, peer agent, human affirming explicitly via `memory_validate`). Cross-session re-emergence is **not** an independent signal — the prior memory is typically already in the new session's stable layer, so re-emission is self-citation with extra steps.

The hidden numeric confidence is updated by mechanical events (validation, time decay, dedup) but never shown to the agent and never used by it for reasoning.

## Consequences

**What becomes easy:**

- The agent reasons about "do I trust this enough to act on it" by reading a state tag, not by interpreting a number. Behaviour is stable across turns.
- Operator audit views are direct: "show me everything tentative" returns rows the operator might want to validate or forget.
- The state machine is small enough to teach in the `<core>` prompt — every agent knows what each rung means.
- Pinning is a binary, not a number ceiling. "Core" memories simply don't demote.

**What becomes hard:**

- Promotion rules require explicit definitions of "independent signal." We carved that out carefully — cross-session re-emergence does not qualify, even though it's suggestive — and documented it. Future signal types must be classified at addition time.
- The numeric confidence is internal-only; we resist exposing it ("just give me one number") because the moment we do, the model starts reasoning about thresholds and the qualitative ladder loses its meaning. Audit dashboards that surface scores are operator-only.

**What we live with:**

- Two representations (state + hidden score) is more machinery than either alone. The cost is worth it for the agent-side stability.
- Passive maturation only reaches Held, never Validated. A long-lived but never-externally-confirmed memory stays at Held forever. Acceptable — that's the rung "I think this is true and nothing has refuted it" deserves.

## Alternatives considered

- **Numeric confidence only.** LLM behaviour is unstable across turns. Rejected.
- **Two rungs only (`tentative` / `held`).** Loses the distinction between "default accepted" and "independently confirmed." Important for operator audits and for the librarian's eviction priorities. Rejected.
- **Five+ rungs.** More granularity than the agent can reason about reliably. Four is the cap; we'd retreat to three if four proves brittle. Accepted as written.
- **Promote on cross-session re-emergence.** Self-reinforcement loop — the same memory ages into Validated without anything actually validating it. Rejected.
