---
name: patom-code-review
description: Review uncommitted code changes in the Patom (patom) project against the project's AGENTS.md conventions, apply confident fixes in place, escalate ambiguous decisions to the human, and produce a final markdown report. Trigger whenever the user invokes /review-patom, asks to "review my changes / working tree / diff", asks for a "AGENTS.md compliance check", or otherwise wants a pre-commit / pre-PR review of in-progress Patom code. Use proactively whenever the user signals they are about to commit or push Patom changes and wants them checked first.
---

# patom-code-review

Review the **uncommitted working-tree changes** in the Patom (`patom`) repo against the project's binding engineering rules in `AGENTS.md`, apply confident fixes directly, escalate the rest, and produce a single markdown report at the end.

This skill is opinionated and scoped to **this project**. It is not a general-purpose code reviewer — its job is to be the second pair of eyes that catches the things `AGENTS.md` says are review-blocking before they reach a PR.

## Why this exists

Patom's `AGENTS.md` is binding, not advisory. Several rules in it are explicit "review-blocking" — bare `String` for an ID, `unwrap()` in non-test code, SQL string concatenation, recursion, `println!`, missing newtypes, untyped errors crossing module boundaries. These are easy to miss while writing a feature and easy to spot on a focused second pass. The goal here is to do that pass *before* the diff lands in a PR, fix what can be safely fixed, and surface the rest with enough context that the human can decide in seconds.

The skill should also resist the temptation to over-mechanize. A finding like "this function is 71 lines, the cap is 70" is real but cheap to ignore; a finding like "this new `parse_agent_id` looks like the existing `AgentId::try_from` and probably should be consolidated" needs human judgement. Aim for the latter.

## Inputs

- **Scope of review**: uncommitted working-tree changes in the current branch. Concretely:
  - `git status --porcelain` to enumerate modified, added, and untracked files
  - `git diff` for unstaged changes
  - `git diff --cached` for staged changes
  - For untracked files (`??` in status), read them in full
- **Authoritative rules**: the repo's `AGENTS.md` at the working-tree root, read fresh on every invocation. The `SPEC.md` it references may or may not exist — if it does, consult for data-model context, but never let its absence block the review.
- **Existing code**: the rest of the workspace, for two reasons:
  1. To search for similar patterns when a new function is introduced (de-duplication and generalization).
  2. To check that touched code stays consistent with conventions established elsewhere in the file or module.

## Workflow

Run these phases in order. Don't skip ahead — phase 1 sets the rubric, and skipping it leads to generic review comments that miss the project's actual rules.

### Phase 1 — Load the rubric

1. Read `AGENTS.md` in full. Don't summarize from memory; the file is short (~260 lines) and changes occasionally.
2. If `SPEC.md` exists, skim its table of contents so you can recognize when a finding touches a SPEC-defined concept (sessions, leases, hooks, idempotency, tenancy).
3. Note any project-specific conventions visible in the touched files' neighbors that aren't in AGENTS.md — for example, a module's preferred naming pattern, its error-type layout, how it constructs its tracing spans. These count as conventions too.

### Phase 2 — Enumerate the diff

1. `git status --porcelain` — get the file list.
2. `git diff` and `git diff --cached` — get the actual changes. For large diffs, read per-file rather than dumping the whole thing.
3. For untracked files, `Read` them whole. New files often introduce more rule-relevant decisions (new modules, new error enums, new newtypes) than edits to existing ones.
4. Group changes by subsystem (e.g. `src/mcp/oauth/*`, `src/http/routes/*`, `migrations/*`, `web/src/*`). This makes the report easier to read and helps spot cross-file inconsistencies.

### Phase 3 — Review against AGENTS.md

For each changed Rust file, walk through the AGENTS.md sections and check for violations. The checklist below is non-exhaustive — use it as a prompt, not a ceiling. When in doubt about whether something is a violation, read the relevant AGENTS.md section verbatim again and decide.

**§1 Types encode invariants**
- New IDs declared as `String` or bare `Uuid` instead of a newtype.
- New bounded numerics as bare `u8`/`u16`/`u32`/`usize` instead of a `TryFrom`-gated newtype.
- Public fields on newtypes (`pub struct Foo(pub Inner)`) instead of `as_*` / `get` readers.
- Boundary types deserialized without `#[serde(try_from = "...")]` — i.e. `serde` skipping the smart constructor.
- `bool` + `Option<T>` shapes that should be a sum type.

**§2 tracing + OpenTelemetry**
- `println!`, `eprintln!`, `dbg!`, or the `log` crate anywhere outside `#[cfg(debug_assertions)]`.
- Span names with interpolated values, or attribute keys not under `patom.*`.
- Errors logged via `tracing::error!` without `error = ?e`, or status set separately from the event.
- `span.in_scope` straddling an `await`.

**§4 Control flow**
- Any recursion, including `async` recursion via `Box::pin`.
- Functions over 70 lines (closures counted).
- New traits/generics for one-shot use (rule of three).
- Conditions stated negatively when they could be positive.

**§5 Limits**
- `loop {}` with no counter-based break, or `for`/`while` without a stated cap.
- `unbounded_channel`, or `broadcast::channel` with an unreasoned size.
- `await` against I/O without a `tokio::time::timeout`.
- Magic numbers in logic instead of named constants in a `limits.rs`.

**§6 Assertions**
- `unwrap()` outside `#[cfg(test)]` — review-blocking.
- `expect()` without a justification message.
- `debug_assert!` in non-test code.
- Non-trivial functions with zero assertions.

**§7 Strict lints**
- `unsafe` blocks (forbidden without an `#[allow]` + safety proof).
- `as` for narrowing/sign-changing casts.
- `#[allow(dead_code)]` / `#[allow(unused)]` without an issue link.
- `Rc<RefCell<...>>` reachable from `async`.
- `tokio::spawn` whose `JoinHandle` is dropped.
- `Box<dyn Error>` or `String` as a returned error type across a module boundary.

**§8 Dependencies**
- New entries in `Cargo.toml` `[dependencies]` without a justification paragraph in the diff or PR.

**§10 SQL**
- `format!` / `+` / `write!` into a SQL string. Review-blocking.
- Runtime dynamic identifiers from untrusted sources.

**§11 Clock**
- New direct calls to `Instant::now`, `SystemTime::now`, `Utc::now`, or `tokio::time::sleep` in production code. Should take a `Clock` instead.

**§12 Errors**
- `anyhow::Error` in a library/module signature (only `main.rs` / top-level glue).
- A module growing a second error enum, or `From` conversions that hide real failure modes behind `#[error(transparent)]`.
- `panic!` / `unreachable!` across a module boundary (assertions excepted).

**§14 Migrations**
- New migration without a paired reversible down migration.
- Edits to a previously-merged migration.
- Online schema changes (NOT NULL backfills, type changes, non-`CONCURRENTLY` indexes) without a rollout note.

**Front-end / TypeScript** — AGENTS.md is Rust-only, but apply the spirit: no `any`, no magic strings for IDs that have a typed counterpart server-side, no silent type assertions.

### Phase 4 — Look for duplication and generalization opportunities

For every **new function** introduced in the diff:

1. Extract its name and a one-line summary of what it does.
2. Search the workspace for similar patterns: `rg -n 'fn <similar_name>' src/`, plus a wider search for the function body's distinctive call sequence (e.g. the specific sqlx + serde + error-wrap pattern it uses).
3. If you find a sibling that does substantively the same thing:
   - **Easy refactor** (one obvious generalization, no behavioural change): apply it as part of the fix pass and note it in the report.
   - **Hard refactor** (touches many call sites, changes a public signature, or the two siblings differ in a subtle way): do **not** refactor. Escalate to the human with both code locations and a recommendation.
4. If you find no sibling, that's fine — record nothing.

The goal is to prevent the codebase from growing two parallel implementations of the same idea, which is much harder to undo later.

### Phase 5 — Categorize findings

Sort every finding into exactly one of three buckets:

- **AUTO-FIX**: mechanical, low-risk, no semantic ambiguity. Examples: replacing `println!` with `tracing::info!`, wrapping a raw `Uuid` field in an existing newtype, deleting an `#[allow(dead_code)]` on a function that is in fact used, swapping `unwrap()` for `expect("invariant: …")` in a place where the invariant is obviously the function's precondition.
- **ESCALATE**: needs the human. Examples: introducing a new newtype (naming + module placement matters), choosing whether to generalize two similar functions, deciding how to express a new error variant, anything that changes a public API or crosses subsystem boundaries.
- **CONFIRM**: the diff is fine — this category exists so the report can affirmatively note compliance for each major AGENTS.md section that the diff touched, rather than only listing problems.

Be biased toward **ESCALATE** when uncertain. A wrong auto-fix is worse than a finding the human has to read.

### Phase 6 — Apply auto-fixes

1. Apply each AUTO-FIX with `Edit`. Keep edits surgical — do not opportunistically reformat or rename around the change.
2. After all edits, run the relevant AGENTS.md §3 gates on the touched code:
   - `cargo fmt --all -- --check`
   - `cargo clippy --all-targets --all-features -- -D warnings`
   - `cargo check --all-targets --all-features`
   - **Do not run** the full `cargo test --all-features` suite unless the user asked for it — it takes 1–2 minutes and isn't always needed for a pre-commit pass. Mention in the report that tests were not run.
3. If a gate fails, **investigate and fix** rather than backing out — but if the failure pre-existed the auto-fix, note it as an escalation instead of attempting an open-ended repair.

### Phase 7 — Write the report

Emit a single markdown report directly in the chat (no file written). Use this structure:

```
## Patom code review — <branch>

**Scope**: <N> files, <X> insertions / <Y> deletions in working tree
**Auto-fixes applied**: <count>
**Escalations**: <count>
**Gates**: fmt ✅ · clippy ✅ · check ✅ · tests skipped

### Applied fixes
- `path/to/file.rs:42` — wrapped `tenant_id: Uuid` in `TenantId` newtype (§1)
- `path/to/other.rs:117` — replaced `println!("…")` with `tracing::info!(…)` (§2)
…

### Escalations
1. **`src/mcp/oauth/flow.rs:88` — duplicate of `pg_store.rs:34`?**
   Both build the same `(client_id, scopes, redirect_uri)` tuple and call `validate_scopes`. Options:
   - (a) extract a `ClientScopeRequest` struct in `oauth/mod.rs`,
   - (b) leave as-is (the validation logic differs subtly in error mapping).
   Recommendation: (a). Needs your call on whether to widen the public surface of `oauth::mod`.

2. **`src/http/routes/mcp.rs:202` — new function `parse_callback_state`**
   Looks similar to `crate::mcp::oauth::flow::decode_state`. If they're the same, consolidate; if they intentionally diverge, add a comment explaining why.
…

### Compliance check (touched sections only)
- §1 Types: ✅ — all new IDs use existing newtypes
- §2 tracing: ⚠️ — see fix #2 above
- §4 Control flow: ✅
- §6 Assertions: ⚠️ — `flow.rs` `exchange_code` has 38 LOC and zero assertions; consider adding pre/post checks (escalation #3)
- §10 SQL: ✅ — new queries use `sqlx::query!`
- §14 Migrations: ✅ — `0037` and `0038` both have paired down migrations

### Not reviewed
- Front-end `.tsx` changes — AGENTS.md is Rust-only; spot-checked for obvious issues, none found.
- Test files — reviewed for §3 shape only.
```

Adapt the template to what the diff actually contains. Sections with nothing to say should be omitted, not left blank with "n/a". The report should be skimmable in under a minute.

## Operating notes

- **Always re-read `AGENTS.md` first.** It's the rubric. Even if you reviewed Patom yesterday — read it again now.
- **Prefer evidence over assertion.** Every finding cites `file:line` and the AGENTS.md section number. "This looks wrong" without a citation is not a finding.
- **Don't refactor the world.** The AGENTS.md rule "one logical change per PR" applies to you too. If the diff is about MCP OAuth, don't reformat the Slack module on the way through.
- **Respect the pre-launch posture.** This project follows a no-backcompat policy pre-launch; do not suggest "add an `Option` shim for backward compatibility" style fixes (see [[feedback_no_backcompat]]).
- **If `cargo test` was already running or recently passed, don't re-run it.** Tests in this repo normally take 1–2 minutes; longer means something's hung (see [[feedback_test_runtime]]).
- **Trust the gates.** If `clippy` is clean after your auto-fixes, that's a strong signal — don't manually re-verify lint rules clippy already enforces.
- **One report at the end.** Don't narrate per-file findings into chat as you go; collect them and emit one structured report. The user wants to read once, not scroll.

## When to escalate vs. fix

If you find yourself writing more than ~3 lines of justification for an auto-fix, it's probably an escalation. Auto-fixes should be obvious enough that the explanation in the report is one line. Anything that needs a paragraph of "I chose X over Y because…" belongs in the escalation list, where the human can weigh in before the change is made.
