# Contributing to Patom

Thanks for considering a contribution. This document covers how to get a development environment running, the rules your patch must follow, and how to land it.

Before anything else: read [`CLAUDE.md`](./CLAUDE.md). It's the engineering ruleset for this codebase and it's **binding, not advisory**. PRs that violate it get bounced regardless of how good the idea is.

---

## Code of conduct

By participating you agree to the [Code of Conduct](./CODE_OF_CONDUCT.md). Be excellent to each other.

---

## Before you start

- **Big changes — open an issue first.** New subsystems, new dependencies, schema changes, or anything that touches multiple modules deserves a design discussion before code. A 20-line issue beats a 2000-line PR that goes the wrong way.
- **Small changes — just send a PR.** Typos, doc fixes, a focused bug fix with a regression test, an additional limit on a loop that didn't have one. Don't ask permission for the obvious.
- **Security issues — do not open a public issue.** See [`SECURITY.md`](./SECURITY.md).

---

## Development setup

```bash
# Clone
git clone https://github.com/tomkapa/patom
cd patom

# Start Postgres
docker compose up -d

# Run the test suite (gates) before you start changing things,
# so you know the baseline is green.
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
```

Web UI:

```bash
cd web
bun install
bun run dev
```

Environment variables are documented in `crates/patom-core/src/config.rs` (the `Settings` struct). Required for most workflows:

- `DATABASE_URL`
- `ANTHROPIC_API_KEY` and/or `OPENAI_API_KEY`
- `PATOM_JWT_SECRET`
- `PATOM_ORG_KEK`

---

## Repository layout (open-core)

Patom is a Cargo workspace with three crates:

- **`crates/patom-core`** — the product library (agents, sessions, MCP, auth, orgs, …). FSL-licensed; its import name is `patom`. Core migrations live in `crates/patom-core/migrations`.
- **`crates/patom-server`** — the binary / composition root. Produces the `patom` binary. Depends on `patom-core` always and `patom-cloud` only under its `cloud` feature. (The binary lives here rather than in `patom-core` to avoid a `patom-core → patom-cloud → patom-core` dependency cycle.)
- **`crates/patom-cloud`** — paid-tier / billing code (entitlement impl, Lemon Squeezy). Source-available under the same FSL, but compiled **only** under `patom-server`'s `cloud` feature, so `cargo build` (the default) never links it. CI fails if it leaks into the default binary.

Where does my code go? If a free-tier self-hoster shouldn't run it, it belongs in `patom-cloud`. The seam between free and paid is a **trait in core**; the paid implementation lives in `patom-cloud`. See [`crates/patom-cloud/README.md`](./crates/patom-cloud/README.md).

**Dependencies** are declared once in the root `[workspace.dependencies]` (version + features + the §8 justification). Member crates only enable them with `name.workspace = true` — never pin a version in a member `Cargo.toml`.

---

## The rules (the short version)

Long version: [`CLAUDE.md`](./CLAUDE.md). Highlights you will trip over otherwise:

1. **Types encode invariants.** Bare `String` / `Uuid` / `u32` for a value with any business meaning is a review-blocking bug. Newtype it with a `TryFrom` smart constructor.
2. **`tracing` + OpenTelemetry only.** No `println!`, `eprintln!`, `dbg!`, or `log` crate. Span names are stable + low-cardinality; dynamic values go on `patom.*` fields.
3. **TDD.** Write the failing test first. The PR's first commit should be the test, the second commit (or same commit) the implementation. PRs without a preceding test are reverted.
4. **No recursion.** Replace with a bounded explicit loop. Same for async recursion.
5. **Every loop has a bound. Every channel is bounded. Every I/O `await` has a timeout.** Limits live in `<module>/limits.rs` with a doc comment explaining the number.
6. **`unwrap()` and `panic!` outside tests are banned.** `expect("invariant: …")` is allowed as a named assertion when the invariant is established within the function.
7. **One error type per module.** `thiserror` enum; `anyhow` only in `main.rs`. `Box<dyn Error>` across a module boundary is banned.
8. **No string concatenation into SQL.** Use `sqlx` bound parameters. Dynamic identifiers go through an allowlist.
9. **Tests own the clock.** Production code takes a `Clock`; tests use a deterministic fake.
10. **Zero-dep bias.** Adding a runtime dep requires a paragraph in the PR explaining why we can't do <200 LOC in-tree, and who owns the upgrade cadence.
11. **Strictest clippy.** Workspace lints are in `Cargo.toml`. CI runs with `-D warnings`. Fix the code or justify the lint inline with a PR-anchored comment.

---

## Exit gates

Every PR must pass, locally and in CI:

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo check --all-targets --all-features
cargo test --all-features
cargo deny check        # CI
cargo audit             # CI
```

Any gate red → the task is not done. No commit, no PR, no "done".

---

## PR hygiene

- **One logical change per PR.** Mechanical refactors (rename, move, `cargo fmt`, edition bump) go in their own PR.
- **Title format:** `area: imperative one-line summary` — e.g. `mcp: scope tool allowlist by agent id`, `runtime: cap retrieval batch at MAX_RETRIEVAL`. Lowercase, no trailing period.
- **Description answers three questions:** *what changed, why now, what could break.* If the answer to "what could break" is "nothing", say so explicitly — that's a meaningful claim.
- **Migrations are paired.** Every schema change has a forward `up.sql` and a tested reversible `down.sql`. Never edit a merged migration; never squash. Online migrations (NOT NULL on a large table, non-CONCURRENTLY index, column-type change) require a written rollout plan in the PR.
- **Reference issues with `Closes #N` / `Refs #N`** so the queue stays accurate.

---

## Commit messages

Conventional-ish, lowercase, imperative. Body explains why the change is needed; the diff explains what changed.

```
runtime: bound thread-stream fan-out by depth, not by row count

Depth is the SPEC-level limit; row count was a proxy that drifted
once memory reads landed inline. The new check uses Depth::CAP
directly and asserts on entry, matching SPEC §Retry.

Refs #142
```

Do not co-author with Claude / any LLM unless the user-facing project says to. Sign-offs and tags follow the GitHub norm.

---

## Tests

- One behaviour per test. The test name describes the behaviour, not the implementation.
- Integration tests hit real Postgres via `#[sqlx::test]`. Mock only paid external services and external HTTP.
- Coverage target: 80 % overall lines, 100 % on the hook evaluator, the per-agent lease manager, and idempotency-key generators.
- For anything time-sensitive: `#[tokio::test(start_paused = true)]` plus `tokio::time::advance(...)`. Flaky real timers are the single biggest waste of debugging hours.

---

## Adding a dependency

Adding a runtime dep is not free. Your PR must include a paragraph (next to the dep in `Cargo.toml`, as the existing entries do) answering:

1. What concrete capability we need.
2. Why an in-tree implementation under ~200 LOC is not appropriate.
3. Who owns upgrade cadence and how we'd remove it if we had to.
4. Feature flags trimmed to the minimum (`default-features = false`).

Dev deps clear a lower bar but still need justification if they pull a large transitive tree.

---

## Documentation

- Engineering rules: `CLAUDE.md`.
- Design notes / future plans: `doc/`.
- Per-module rationale: doc comments at the top of `mod.rs`.
- Don't write user-facing tutorials in the repo until the surface is stable enough to commit to.

---

## Licensing of contributions

Patom is licensed under [FSL-1.1-Apache-2.0](./LICENSE.md) — source-available with a 2-year Apache-2.0 future grant and a non-compete on running it as a substituting commercial service.

By submitting a contribution (PR, patch, doc edit, anything) you agree that:

1. Your contribution is licensed under the same FSL-1.1-Apache-2.0 terms as the rest of the project.
2. You have the right to submit it under that license (i.e. it's your work, or you have permission from the rights holder).
3. You understand the Future License clause means your contribution will become Apache-2.0 licensed two years after the release it ships in.

No CLA is required for now — submission is sufficient assent. If that changes the maintainers will give notice and a grandfather window.

---

## Releasing

Not relevant yet — Patom is pre-1.0 and is not published to crates.io. Once that changes this section will describe the release flow.

---

## Questions

Open a GitHub Discussion (preferred) or a low-priority issue. For private questions, use the security contact in [`SECURITY.md`](./SECURITY.md).
