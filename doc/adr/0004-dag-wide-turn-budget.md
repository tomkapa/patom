# ADR-0004 — DAG-wide turn budget, not depth cap

- **Status:** Accepted
- **Date:** 2026-05-15
- **Deciders:** core

## Context

When agent A can ask agent B for help, and B can ask C and D in parallel, the call graph is a tree (or, with sessions that resolve to canonical pairs, a DAG). Two failure modes follow:

1. **Runaway depth.** A → B → C → D → … with no obvious end condition. Any chat-loop bug or model misfire could spawn unbounded delegations.
2. **Runaway fan-out.** A asks ten agents in parallel; each of those asks five; suddenly fifty turns are in flight for one human prompt.

We needed a structural cap. Two shapes were on the table:

- **Depth cap** — `MAX_DEPTH` from root. Simple, but pessimistic about wide-but-shallow fan-out (a coordinator agent reasonably asks five specialists in parallel) and permissive about deep narrow chains (A → A' → A'' through three "thin" reframings).
- **Total turn budget per DAG** — `MAX_DAG_TURNS` across the whole tree. Treats wide parallel work the same as deep chains, both of which cost the same amount of compute and time.

## Decision

**A DAG (rooted at a single human-initiated session) is bounded by a per-tree total turn budget — `MAX_DAG_TURNS` — not by depth.**

- Every `send_message` to an agent atomically bumps the `prompt_request_dags.turns_used` counter and checks it against `turns_cap`.
- When the budget exceeds, the offending `prompt_requests` row is **left in the database** (status = failed with the budget-exceeded reason) so engineers can see exactly which message broke the loop, not just that "the DAG halted."
- Reflection and resolution turns are off-DAG — they don't consume the budget.
- The budget is per-DAG, not per-agent and not per-tenant. Per-agent / per-tenant quotas are a separate axis (see [ADR-0007](./0007-tenancy-rls-and-envelope-encryption.md)).

## Consequences

**What becomes easy:**

- A coordinator agent can legitimately fan out to ten specialists in parallel within a 50-turn budget — depth-based caps would have made this infeasible without per-role tuning.
- The dropped message stays auditable. "Why did this conversation halt?" has a row to point at, not "the runtime gave up."
- Quiescence detection becomes well-defined: a DAG is quiescent when `turns_used <= turns_cap` AND no requests are `pending` or `processing`. The thread SSE stream uses this to emit a synthetic terminal event.

**What becomes hard:**

- Picking `MAX_DAG_TURNS` is a product judgement, not a mechanical derivation. Set too low: legitimate fan-outs hit the wall. Set too high: a broken agent can burn provider tokens before halting. Named in `runtime/limits.rs` with a documented "why this number."
- Wide fan-out with shallow result-merging means N turns of model calls finish at roughly the same time and hit the database concurrently. Acceptable today (bounded by `MAX_WORKERS`); a future split into a separate worker tier may need rate-limiting around the merge point.

**What we live with:**

- A pathological model behaviour ("ask the same agent for the same thing 49 times") consumes the budget without producing useful output. The poison-cap (per-row `attempts`) handles single-row retries; multi-row identical-prompt floods are an open observability problem that we plan to address with per-DAG provider-token spending caps later (out of scope for this ADR).

## Alternatives considered

- **Per-agent recursion depth cap.** Doesn't compose well — A asking B in a single hop counts the same as A → B → C → D. Rejected.
- **No structural cap; rely on hooks for enforcement.** Hooks can deny individual calls but have no global view of the DAG. Cap would have to be reimplemented inside the hook. Rejected.
- **Provider-token budget (e.g. "this DAG can spend at most N tokens").** Hard to make atomic and idempotent across crash recovery. A reasonable future addition layered on top of the turn budget, but not as the primary cap.
- **Move-on-exceed (mark dropped messages as warn, continue).** Hides the breakage. Better to fail loud and let the operator see the conversation that broke.
