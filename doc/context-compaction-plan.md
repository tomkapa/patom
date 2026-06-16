# Patom — Context compaction (bound unbounded thread context per turn)

> Status: **shipped** (#182). Authored 2026-06-16. Closes the deferral in [`lark-byo-integration-plan.md` §Deferred](lark-byo-integration-plan.md): *"the full mirror flows unbounded into context today; this is accepted and will be handled by the planned compaction feature."* Tracks [#182](https://github.com/tomkapa/patom/issues/182).
>
> **As-built deltas from this plan** (the body below is the original design; these are the decisions that changed during implementation): the summarizer **reuses the turn's already-resolved `(provider, model)`** — there is no separate `SUMMARIZER_MODEL` registry lookup; the trigger/cut constants are `CONTEXT_TOKEN_BUDGET_DIVISOR` (budget = `window / 2`) and `MAX_TOOL_RESULT_CHARS` (the old `COMPACTION_THRESHOLD` / `MAX_TOOL_RESULT_TOKENS` names were dropped); and compaction folds are metered under a dedicated `MetricKind::Compaction` (a `turn_metrics`-only label, kept out of the queue-dispatch `RequestKind`).
>
> **Principles:** (1) **correctness/safety first** — the prompt is bounded on *every* turn regardless of whether summarization succeeds; (2) **the shared feed is immutable** — `thread_messages` is the system of record; compaction is a *derived* artifact the read path consults, never a rewrite of history; (3) **single path** — no per-platform branching; this is a core feature, not Lark-specific (though Lark's ambient group-message ingest raises its priority); (4) **CLAUDE.md binding** — bounded loops, no recursion, newtype invariants, tests own the clock, one error type per boundary, saturation metrics.

## Why / positioning

Thread context flows into the model prompt **unbounded** today. `context_for_agent` ([pg_store.rs:572](../crates/patom-core/src/threads/pg_store.rs)) runs `WHERE thread_id=$1 AND (kind='posted' OR owner_agent_id=$2) ORDER BY seq ASC` with **no `LIMIT`**, and `build_thread_request` ([turn.rs:260](../crates/patom-core/src/agent_core/turn.rs)) hands the full `Vec<ChatMessage>` straight to the provider. On a long thread this overflows the context window, inflates token cost, and degrades quality. Lark's ambient ingest mirrors **every** group message (mention or not), so threads grow far faster than the web/Slack-mention-only case.

## How the field does it (research, for reference)

All mainstream coding agents converge on **protected head + summarized middle + verbatim tail**, triggered on a token threshold:

| Agent | Trigger | Tail kept | Summary | Tool pairs |
|---|---|---|---|---|
| Claude Code | ~95% of window | server-managed | opaque: accomplishments / WIP / files / next / constraints | — |
| Codex CLI | ~90% cap | ~20k tokens | "handoff for another LLM" | — |
| Openclaw | `tokens > window − reserve` | ~20k tokens | **hierarchical**, prev summary fed forward; tool result capped at 30% window | cut never splits a pair |
| Hermes | 50% + 85% safety net | first-3 + `protect_last_n` | **structured template**, *updated* not regenerated | ordering-aware |

Four lessons we adopt: trigger on tokens (not message count); **cut-points must never split a `tool_use` from its `tool_result`**; the summary is **rolling** (fold the previous summary into the next compaction); **tool results are the heaviest payloads** and need separate, *non-destructive* handling (a companion feature — see [§Tool-result reduction](#tool-result-reduction--companion-feature)), not blind truncation.

Sources: [Claude Code auto-compact](https://claudelog.com/faqs/what-is-claude-code-auto-compact/), [compaction research gist](https://gist.github.com/badlogic/cd2ef65b0697c4dbe2d13fbecb0a0a5f), [Hermes vs Claude Code (mem0)](https://mem0.ai/blog/how-hermes-and-claude-handle-context-compression-in-real-production-agents-(and-what-you-should-extract)), [Building Openclaw — compaction](https://systemdesigner.medium.com/building-openclaw-from-scratch-part-5-conversation-compaction-c467e41f926f).

## Why we can't port it verbatim

Those agents own a **linear, single-owner session** they can mutate (replace old turns with a summary in the session store). Patom does not:

- Context is **assembled fresh per turn** from a **shared, multi-writer feed** — it is *derived*, not a stored working set, so we can't overwrite old rows.
- It is **per-agent perspective-mapped**: each agent sees `posted` messages **plus only its own** private `reasoning`/`tool_use`/`tool_result` (`owner_agent_id` filter). The natural compaction grain is therefore **per-(thread, agent)**, which is *exactly* the grain `context_for_agent` already queries.
- **No pre-request token counting exists** — `Usage{input_tokens,…}` is recorded only *after* the response ([turn.rs:610](../crates/patom-core/src/agent_core/turn.rs)); the model catalog has **no context-window metadata** ([catalog.rs](../crates/patom-core/src/provider/catalog.rs)).
- The only caps today are UI-facing (`MAX_THREAD_FEED=500`); nothing bounds context.

## Locked decisions

| Fork | Decision | Consequence |
|---|---|---|
| **Grain** | **Per-(thread, agent)** | One `thread_compactions` row per `(thread_id, agent_id)`; the summary folds in that agent's *own* private tool/reasoning trail too. Matches the existing read filter exactly. |
| **Trigger** | **Inline on overflow** | The summarize pass runs synchronously inside the turn that overflows, under timeout. No background worker. Self-contained correctness. |
| **Phasing** | **Single combined PR** | Windowing floor + rolling summary land together, built test-first in staged commits (below). |

## Design — rolling per-(thread, agent) summary, consulted at read time

Two layers. The **windowing floor** is the correctness guarantee (no LLM, always holds). The **rolling summary** is the fidelity layer (LLM, best-effort).

```text
build path (agent_core, has the provider):
  comp   = threads.load_compaction(thread, agent)        # summary + covers_through_seq | none
  since  = comp.covers_through_seq | 0
  tail   = threads.context_tail(thread, agent, viewer, since, overrides)   # seq > since, ordered, repaired
  est    = estimate_tokens(comp.summary, tail)            # real input_tokens feedback + chars/4 cut heuristic
  if est <= CONTEXT_TOKEN_BUDGET and tail.len() <= MAX_CONTEXT_MESSAGES:
      summary = comp.summary                              # COMMON PATH — no LLM
  else:
      overflow, keep = cut_at_tool_safe_boundary(tail)    # leave the last window verbatim; never split a pair
      summary = compactor.summarize(comp.summary, overflow)   # rolling; chunked; bounded; timeout
                  .unwrap_or(comp.summary + truncation_note) # SAFETY: failure still bounds the prompt
      threads.save_compaction(thread, agent, summary, covers_through_seq = overflow.last.seq)
      tail = keep
  request.system   += render_summary_section(summary)     # protected head
  request.messages  = tail                                # bounded verbatim tail
```

**Layering:** the LLM call lives in `agent_core` (where the provider is), **not** in the storage layer. `pg_store.rs` gains only pure-SQL primitives (`load_compaction`, `context_tail(since_seq)`, `save_compaction`). This keeps storage free of provider calls.

**Summary injection:** into the **system prefix** (the protected head), as a `## Earlier conversation (compacted)` section appended in `build_thread_request` next to the memory/todos fold. The verbatim tail stays the `messages` vec. Chronology is preserved (summary = old, tail = recent). Prompt-cache churn is limited to the rare overflow turn.

### Read path — `threads/pg_store.rs`

- Generalize `context_for_agent` into `context_tail(thread, agent, viewer, since_seq, overrides)` — same query + perspective-map + `repair_tool_pairs`, with an added `AND m.seq > $since` and an `ORDER BY seq ASC LIMIT MAX_CONTEXT_MESSAGES + tool-pair slack`. The hard `LIMIT` is the floor: even with **no** summary yet (first turn on a pre-existing 500-message thread) the prompt cannot be unbounded.
- **Oversized `tool_result` bodies are capped for the *prompt only*, never destructively.** The full row always stays in the immutable `thread_messages` feed; at assembly time a body over `MAX_TOOL_RESULT_CHARS` is rendered as `head + [… omitted N chars · thread seq {seq}] + tail` so it remains recoverable, not deleted. This is a thin safety net — semantic, produce-time reduction of heavy tool results lives in the **companion feature** ([§Tool-result reduction](#tool-result-reduction--companion-feature)).
- `load_compaction` / `save_compaction` — bound-parameter SQL only (CLAUDE.md §10), `#[derive(FromRow)] + TryFrom<Row>` at the boundary.
- Return type becomes a struct, not a bare `Vec`:
  ```rust
  pub struct AgentContext {
      pub summary: Option<CompactionSummary>,
      pub messages: Vec<ChatMessage>,
  }
  ```

### Compaction step — `agent_core/compaction.rs` (new module)

- A `Compactor` that summarizes with the **turn's already-resolved `(provider, model)`** (as-built; the original plan resolved a separate `SUMMARIZER_MODEL`, but the locked decision is to reuse the agent's own model — no second registry lookup).
- **Chunked, bounded, non-recursive** (CLAUDE.md §4/§5):
  ```text
  fn summarize(prev: Option<CompactionSummary>, overflow: Vec<ChatMessage>) -> Result<CompactionSummary, CompactionError>:
      chunks = split(overflow, SUMMARIZER_INPUT_BUDGET)          # Vec, oldest first
      assert!(chunks.len() <= MAX_COMPACTION_CHUNKS)             # if larger, drop oldest + log (no silent cap)
      acc = prev.unwrap_or_else(empty_template)
      for chunk in chunks:                                       # bounded loop, ≤ MAX_COMPACTION_CHUNKS
          acc = fold(acc, chunk).await?                          # one provider.send under COMPACTION_LLM_TIMEOUT
      Ok(acc)
  ```
- **Structured rolling template** (Hermes lesson) — the fold prompt asks the summarizer to *update* sections, not regenerate:
  `Facts · Decisions · Constraints · Open items · Progress`. Previous summary passed in as the base.
- **Realistic worst case:** because `covers_through_seq` advances each overflow, steady-state `overflow` is ~one window's worth (1–2 chunks). The multi-chunk path only fires on the first compaction of a pre-existing huge thread; the `MAX_COMPACTION_CHUNKS` cap bounds even that, dropping the oldest beyond the cap with a `WARN` + metric.

### Token estimation & model window — `provider/catalog.rs`

- Add `context_window: u32` to `CatalogEntry` + a `ContextWindow(u32)` newtype (CLAUDE.md §1). `CONTEXT_TOKEN_BUDGET` is a fraction of it.
- **Trigger signal uses real data, zero new deps:** read the previous turn's `input_tokens` from `turn_metrics` (already keyed by `state_id`) as the "are we near the window" signal. The `chars/4` estimator (`TokenEstimate(u32)` newtype) is used only to pick the **cut-point** within the tail. A `patom.context.tokens_estimated` vs actual-`input_tokens` comparison metric calibrates the heuristic. No tokenizer crate (CLAUDE.md §8).

## Data model — migration `00000000000084_thread_compactions`

```sql
-- up
CREATE TABLE thread_compactions (
    thread_id          UUID        NOT NULL,
    agent_id           UUID        NOT NULL,
    org_id             UUID        NOT NULL,
    summary            TEXT        NOT NULL,
    covers_through_seq BIGINT      NOT NULL,
    summary_tokens     INTEGER     NOT NULL,
    version            INTEGER     NOT NULL DEFAULT 1,
    updated_at         TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (thread_id, agent_id)
);
-- RLS: scope by org membership, consistent with the rest of the schema.
-- down: DROP TABLE thread_compactions;
```

- `covers_through_seq` = highest feed `seq` folded into `summary` for this `(thread, agent)`.
- One row per `(thread, agent)` — upsert on compaction. Reversible down (CLAUDE.md §13). RLS policy mirrors `thread_messages`; verify the active-org pin (memory: [rls-gates-membership-not-active-org]).

## Limits — `threads/limits.rs` (+ `agent_core/limits.rs`)

Each named, exported, doc-commented with *why this number* (CLAUDE.md §5). Starting values to tune:

| Const | Start | Why |
|---|---|---|
| `MAX_CONTEXT_MESSAGES` | 200 | hard cap on verbatim tail rows — the floor that bounds turn 1 |
| `CONTEXT_TOKEN_BUDGET_DIVISOR` | 2 (budget = window/2) | leave room for system + tools + output; the trigger fires at this budget (as-built: the separate `COMPACTION_THRESHOLD` was dropped) |
| `MAX_TOOL_RESULT_CHARS` | ~32k chars (≈8k tokens) | **prompt-render** cap for one tool_result (full row retained in feed); safety net only |
| `SUMMARIZER_INPUT_BUDGET` | ~24k | per-chunk input to the summarizer |
| `MAX_COMPACTION_CHUNKS` | 8 | bounds the fold loop; over → drop oldest + log |
| `COMPACTION_LLM_TIMEOUT` | 30s | every summarizer `await` is timeout-wrapped (CLAUDE.md §5) |
| `MAX_COMPACTION_WALL_CLOCK` | 90s | total inline budget across folds (as-built addition) |
| `COMPACTION_COOLDOWN` | 5 min | back off a failing summarizer (as-built addition) |
| `MAX_SUMMARY_TOKENS` | ~4k | cap the rolling summary itself so it can't grow unbounded |

## Types & errors (CLAUDE.md §1, §12)

- Newtypes: `ContextWindow(u32)`, `TokenEstimate(u32)`, `CompactionSummary` (wraps the structured text; `TryFrom` enforces `MAX_SUMMARY_TOKENS`), reuse the existing `Seq` for `covers_through_seq`.
- `CompactionError` (`thiserror`) in the new module: `SummarizerTimeout`, `Provider(ProviderError)`, `Empty`, `TooLarge`. Bridged into `AgentError` via `From`. `ThreadError` gains the storage variants. No `String` errors, no panic across boundaries.

## Observability (CLAUDE.md §2 — the issue's "saturation metric")

- `patom.compaction.triggered` — counter (attrs: `patom.agent.id`, `patom.thread.id`).
- `patom.compaction.failed` — counter (summarizer error/timeout → fell back to truncation).
- `patom.context.messages_windowed` — gauge: rows dropped from the tail this turn.
- `patom.context.tokens_estimated` — histogram, compared against recorded `input_tokens` to calibrate `chars/4`.
- A `compaction.summarize` span around the fold loop; on error a `tracing::error!(error = ?e)` event (sets span status ERROR via the OTel bridge).

## Failure & safety semantics

The windowing floor is enforced **before and independently of** summarization:

1. `context_tail` already `LIMIT`s, so even a cold thread with no summary is bounded.
2. If `compactor.summarize` errors or times out, we **fall back to truncation** — keep the verbatim `keep` tail (+ the stale prior summary if any), emit `patom.compaction.failed`, and proceed. The turn never blocks on the summarizer, and the prompt is **never** unbounded.
3. `assert!(!tail.is_empty())` (the cut always leaves the most-recent messages) and `assert!(tail.len() <= MAX_CONTEXT_MESSAGES)` — assert both directions (CLAUDE.md §6). The existing turn.rs:300 non-empty assertion is preserved/strengthened.

## TDD plan (staged commits within the single PR)

Red → green per stage (CLAUDE.md §3); `#[sqlx::test]` against real Postgres (Docker, local); `#[tokio::test(start_paused = true)]` + `TestClock` for timeouts; a deterministic fake summarizer provider (no paid calls).

1. **Catalog window** — `CatalogEntry.context_window` + `ContextWindow` newtype; estimator `chars/4` + `TokenEstimate`. *(unit)*
2. **`context_tail(since_seq)`** — only `seq > since` returned, ordered, `LIMIT` enforced. *(sqlx)*
3. **Windowing floor** — N-message thread, no summary → `messages.len() ≤ MAX_CONTEXT_MESSAGES` and est ≤ budget. *(sqlx)*
4. **Tool-pair-safe cut** — interleaved `tool_use`/`tool_result` fixture → cut never splits a pair. *(unit)*
5. **Heavy tool-result cap is reversible** — oversized body → prompt render capped to `head+id+tail`; the underlying `thread_messages` row is unchanged (a re-read returns the original bytes). *(sqlx)*
6. **`load`/`save_compaction`** round-trip + upsert advances `covers_through_seq`. *(sqlx)*
7. **Inline trigger** — overflow → fake summarizer invoked once; compaction row persisted; next read returns summary + reduced tail. *(sqlx)*
8. **Rolling update** — second overflow → fake provider asserts it *received the prior summary*; `covers_through_seq` advances; summary not regenerated from scratch. *(sqlx)*
9. **Summarizer failure** — fake provider errors/times out → turn still completes, bound still holds, `patom.compaction.failed` emitted, no overflow sent. *(tokio paused)*
10. **Chunked big-bang** — 500-message cold thread → ≤ `MAX_COMPACTION_CHUNKS` folds, oldest-beyond-cap dropped + logged, non-empty summary, bounded prompt. *(sqlx)*
11. **Long-thread coherence** — drive ~300 messages across many turns → prompt size bounded every turn; summary retains early facts. *(integration)*

Coverage: 100% on the cut-point/estimator logic (per CLAUDE.md §3's "100% on the hook evaluator"-class units).

## Exit gates (CLAUDE.md §3)

`cargo fmt --check` · `clippy -D warnings` · `cargo check --all-targets` · `cargo test --all-features` · e2e for the thread surface · `cargo deny` / `cargo audit`. Migration up+down verified against a staging dump (small friends-only prod — full reset acceptable only with explicit approval; default to reversible).

## Tool-result reduction → companion feature

Heavy tool results (web fetch, large file reads, fat MCP payloads) are the single biggest source of prompt bloat — but **blind truncation loses data the agent may still need**. The fix is **semantic reduction at produce-time** (where `tool_use → tool_result` is created), not at context assembly. Split into a **separate issue** that complements #182:

- **Per-tool `ToolResultPolicy`** at the tool-dispatch seam:
  - `Verbatim` — small results, store as-is.
  - `Paginate { page_tokens }` — file/listing/structured reads: return the first page of *real* content + `offset=N` to continue. Lossless, agent-driven, **no LLM cost**. Mirrors Claude Code `Read`.
  - `Summarize` — web fetch / large opaque MCP payloads: a **cheap-model (Haiku) extractive pass keyed to the call's intent**, stored as the visible body. Mirrors Claude Code `WebFetch` ("answers a prompt against the page with a small fast model").
- **`Summarize` reuses the `Compactor` chunk-fold** (split → fold → fold, bounded ≤ `MAX_COMPACTION_CHUNKS`, each under timeout) — i.e. "load big payload → chunk → summarize each chunk until done."
- **Offload substrate (under both):** the full payload → `tool_artifacts(handle, org_id, full_body, tokens, created_at)`; the visible result carries the `handle`; a `read_artifact(handle, offset/limit | grep)` system tool recovers exact slices on demand. **Lossless** — reduced for the prompt, fully addressable.

Until that lands, #182 keeps tool results lossless by **retaining the full row in the immutable feed and capping only the prompt rendering** (see [Read path](#read-path--threadspg_storers)).

## Non-goals / deferred

- **Tool-result *semantic* reduction** — produce-time Paginate/Summarize + offload, as above; tracked as its own issue, not built in #182 (which only renders-caps losslessly).
- **Background compaction worker** (reflection-style, off the turn path) — inline is correct and self-contained; revisit only if overflow-turn latency proves painful.
- **Real tokenizer crate** — `chars/4` + real `input_tokens` feedback first; add a dep only if the calibration metric shows it's needed (CLAUDE.md §8).
- **Cross-session / per-person memory** — separate from thread compaction (the memory subsystem already exists and is out of scope here).
- **Semantic retrieval over old turns** (RAG instead of linear summary) — future enhancement; the rolling summary is the v1.

## Open questions to confirm during build

1. **Summarizer provider resolution** — RESOLVED as-built: the summarizer reuses the turn's already-resolved `(provider, model)` (the same `route()` result), so no independent `SUMMARIZER_MODEL` lookup is needed.
2. **Summary as system-prefix vs leading message** — default to system-prefix (protected head); reassess if it hurts prompt-cache hit-rate (watch `cache_read_input_tokens`).
3. **RLS active-org pin** on `thread_compactions` reads — apply the same fix pattern as `agents.rs` (memory: [rls-gates-membership-not-active-org]).
