# ADR-0008 — Provider-agnostic agent core; everything I/O is a trait

- **Status:** Accepted
- **Date:** 2026-04-29
- **Deciders:** core

## Context

The agent turn loop calls a model provider, persists messages, publishes streaming chunks, invokes tools, and reads/writes memory. Each of those is I/O. If the turn loop owns the I/O directly:

1. **Tests need a live database, a network, and an API key.** Slow, flaky, and impossible to make hermetic.
2. **Switching providers is a code rewrite.** Anthropic vs OpenAI vs a self-hosted llama-cpp model becomes a branch in the loop.
3. **The deployment shape is frozen.** "Run HTTP and worker in the same process" vs "split them" can't be decided per-deployment without reshaping the code.

The opposite extreme — a fully dynamic dependency-injection framework — is over-engineering for a single-binary product.

## Decision

**The agent core (`src/agent_core/`) owns no I/O. Every external dependency — model provider, session store, response sink, hook chain, tool box, memory store, clock — is a trait. The core operates only on traits and pure types.**

- Concrete impls (`PgPromptQueue`, `PgResponseSink`, `AnthropicProvider`, `OpenAiProvider`, `PgMemoryStore`, …) live outside the core and are wired together at `src/app.rs` (the composition root).
- The same code compiles against in-memory fakes (`InMemory*`) for tests and against the Postgres + provider impls for production.
- Adding a new provider is one trait impl + one wire in `app.rs`. No turn-loop changes.
- Splitting the binary into `serve-http` / `serve-worker` later is a composition decision: same code, two compose roots, no trait changes.

Time is special: production code never calls `Instant::now`, `SystemTime::now`, `chrono::Utc::now`, or `tokio::time::sleep` directly. Everything that needs time takes a `Clock`. The production impl is `SystemClock`; tests use `TestClock` with `tokio::time::pause()`. This is binding per CLAUDE.md §11 — flaky real timers are the single biggest waste of debugging hours.

## Consequences

**What becomes easy:**

- Hermetic integration tests. The agent core runs end-to-end against fakes — no live API, no real Postgres, no network — in milliseconds.
- Provider parity. A bug in the Anthropic path that doesn't repro on OpenAI is one provider trait swap away from being isolated.
- The trait shape is the design contract. If a new feature requires widening a trait, that's the signal to discuss before writing impls.

**What becomes hard:**

- A new feature that doesn't fit the existing trait shape requires a trait change in addition to an impl change. This is by design — the friction is the conversation.
- Trait dispatch has a small runtime cost (vtable lookup). Negligible against LLM call latency.
- Composition-root wiring in `app.rs` grows as we add subsystems. We've kept it linear and readable; if it ever stops being that, that's the signal we need a smaller composition seam, not a DI framework.

**What we live with:**

- Trait boundaries that turn out wrong are visible as "every impl has the same shape of workaround." When that happens, the answer is to fix the trait, not to keep working around it (CLAUDE.md §4 — three similar `impl` blocks beat a premature trait, but three similar workarounds inside `impl` blocks are the signal the trait is wrong).
- The agent core cannot call `tokio::spawn`. Background work belongs to the runtime, not the core. If a turn needs a side-effect on a clock, it takes a `Clock` and schedules a write that the runtime acts on.

## Alternatives considered

- **Generic-parameterised core (`fn turn<P: Provider, S: SessionStore>(...)`).** Compiler-level zero-cost dispatch, but the generic explosion across the call tree is unreadable for what is fundamentally a single binary. Rejected.
- **Concrete dependencies, mock the network layer.** Mocking HTTP/SQL at the byte level is brittle and tests the wrong thing. Rejected.
- **DI framework (e.g. `shaku`, `inventory`-style registries).** Over-engineered for our scale; resolution-at-runtime trades compile-time safety for "magic." Rejected.
