# Architecture Decision Records

This directory captures the load-bearing architectural decisions in Patom. Each ADR is short, dated, and stable — once accepted, it is not edited. A decision that proves wrong is replaced by a new ADR that **supersedes** the old one (the old one stays, marked superseded, so the historical reasoning is preserved).

Format follows Michael Nygard's template: **Status · Context · Decision · Consequences**.

## Index

| #     | Title                                                                       | Status   |
| ----- | --------------------------------------------------------------------------- | -------- |
| 0001  | [`send_message` is the only inter-actor channel](./0001-send-message-as-only-channel.md) | Accepted |
| 0002  | [Agents address each other by role name](./0002-agents-address-by-role-name.md)          | Accepted |
| 0003  | [Per-agent private memory with idleness-driven reflection](./0003-per-agent-memory-with-reflection.md) | Accepted |
| 0004  | [DAG-wide turn budget, not depth cap](./0004-dag-wide-turn-budget.md)        | Accepted |
| 0005  | [Per-agent MCP tool scoping enforced in code](./0005-per-agent-mcp-scoping.md) | Accepted |
| 0006  | [Durable queue with session leases](./0006-durable-queue-with-session-leases.md) | Accepted |
| 0007  | [Tenancy via Postgres RLS + per-org envelope encryption](./0007-tenancy-rls-and-envelope-encryption.md) | Accepted |
| 0008  | [Provider-agnostic agent core; everything I/O is a trait](./0008-provider-agnostic-agent-core.md) | Accepted |
| 0009  | [Typed memory states, not numeric confidence](./0009-typed-memory-states-not-numeric-confidence.md) | Accepted |
| 0010  | [Append-only journal for memory + replayable revert](./0010-append-only-memory-journal.md) | Accepted |
| 0011  | [Pluggable OIDC auth; Google is one preset; first login bootstraps the admin](./0011-pluggable-oidc-auth-and-first-admin-bootstrap.md) | Accepted |

## When to write a new ADR

Write one when you are about to **commit to a property** that future code will be expected to respect. Signals:

- A decision changes how a primitive is shaped, named, or addressed.
- A reasonable engineer reading the code would ask "why is it like this?" and the answer is not in the source.
- The decision rules *out* a class of alternatives, and you want to record what was ruled out and why.

Routine refactors, dependency bumps, file moves, and naming choices do **not** need ADRs.

## Template

```markdown
# ADR-NNNN — <Title>

- **Status:** Proposed | Accepted | Superseded by ADR-XXXX | Deprecated
- **Date:** YYYY-MM-DD
- **Deciders:** <names>

## Context

What forces this decision? What was true before? What were we observing?

## Decision

The single sentence first, then unpack it. State what we are committing to.

## Consequences

- What becomes easy.
- What becomes hard or impossible.
- What we have to live with that we wouldn't have chosen in isolation.

## Alternatives considered

Brief — one paragraph each — for the options we did not pick.
```
