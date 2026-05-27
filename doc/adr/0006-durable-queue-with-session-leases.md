# ADR-0006 — Durable queue with session leases

- **Status:** Accepted
- **Date:** 2026-04-29
- **Deciders:** core

## Context

The runtime has to accept work from heterogeneous triggers — a human HTTP prompt, a Slack `@mention`, an inter-agent `send_message`, a scheduled wake-up firing — and run them through the same agent turn loop. Three properties matter:

1. **Durability.** If the worker process crashes mid-turn, the work cannot be lost. The user submitted a prompt; they expect a reply.
2. **Crash-only recovery.** Recovery on restart should be the normal path — no special "recovery mode" code. Whatever runs on a healthy boot also unsticks a crashed-last-time deployment.
3. **No double-fire.** A crashed worker whose lease expired must not produce two assistant turns when the replacement worker picks the row up.

A naive `tokio::spawn` per request and an in-memory `mpsc` queue fails all three. An external broker (Redis, NATS) adds an operational dependency we don't need given that Postgres is already the source of truth for everything else.

## Decision

**The durable inbox is a Postgres table — `prompt_requests` — and concurrency is gated by per-session leases with a monotonic `turn_seq` fence.**

- Every trigger inserts a `prompt_requests` row. `INSERT ... RETURNING id` is the acknowledgement to the caller.
- `session_leases(session_id, leased_by, leased_until, turn_seq)` is the lease table. A worker claims a session by acquiring the lease and incrementing `turn_seq`.
- The claim returns a `LeaseToken { session_id, turn_seq }`. Every subsequent write the worker performs carries the token, and the SQL is gated by `WHERE turn_seq = $token`.
- Lease heartbeat runs at `LEASE_TTL / 3`. If the worker dies, the lease expires; the next claim resets orphan rows (`status = processing, turn_seq < new_seq`) back to `pending` with `attempts++`. After `MAX_ATTEMPTS = 3`, the row is marked `failed` with `reason = poison`.
- Time enters every impl through the `Clock` trait. Tests use `TestClock` with `tokio::time::pause()` for deterministic lease expiry.

A zombie worker writing with a stale `turn_seq` matches no rows — silent no-op. That's the property the fence is for.

## Consequences

**What becomes easy:**

- Crash recovery is the boot path: orphan rows reset on the next claim, exactly like a routine claim of a never-claimed row.
- Idempotency lives on the same table: `UNIQUE (org_id, idempotency_key)` makes `POST /prompts` retries return the original row.
- Adding new triggers (Slack, scheduler, peer agent) is one INSERT — no new transport, no new code path through the worker.
- Worker concurrency is naturally per-session-serialised. A session is processed by one worker at a time; multiple sessions process in parallel up to `MAX_WORKERS`.

**What becomes hard:**

- Polling the queue is flat — no `LISTEN/NOTIFY` for the claim path today. The worker sleeps 1s between empty claims. Acceptable at current scale; if it stops being acceptable, we add NOTIFY on insert without changing the trait shape.
- Postgres becomes the hot path for both control plane (queue) and storage (everything else). Connection pool sizing matters; queries on the queue must stay indexed and bounded.
- Lease expiry trades responsiveness for safety. A worker that hangs (rather than crashes) holds its session for the heartbeat window before the orphan recovery kicks in. We've chosen safety over responsiveness — the hang is a bug worth diagnosing, not an event to mask.

**What we live with:**

- One `prompt_requests` row per turn, even when many turns belong to the same human prompt's DAG. The cost is small per row but the table grows linearly with turn count. Retention/archival policy is a separate decision (not yet binding).

## Alternatives considered

- **In-memory queue with `tokio::mpsc`.** Loses every in-flight turn on crash. Rejected.
- **External broker (Redis Streams, NATS).** Operational dependency we don't have a reason for. Rejected — Postgres is already the SoT.
- **Optimistic concurrency on `prompt_requests` without an explicit lease table.** Conflates "claim a row" with "claim the session." Two rows on the same session could be claimed by different workers, producing interleaved turns. Rejected — leases are session-level, not row-level.
- **No `turn_seq` fence; rely on `leased_until` time check.** A worker whose clock drifts could write after its lease expired but before the new worker checks the time. The fence makes the check ordering-based, not time-based. Accepted as written.
