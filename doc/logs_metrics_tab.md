# Logs & Metrics tab

How a customer audits an agent's runtime behaviour and tunes its
prompt. Lives under `/agents/:id/logs` — the fourth nav item in
`AgentLayout`, currently stubbed `disabled: true`.

This is a **customer-facing** tab. Everything here renders for the
end user; nothing assumes operator/site-admin tools, external
observability backends, or trace-id deep links. If the answer cannot
be built from rows the customer's org owns, it does not appear here.

This document is the **design** — what the tab shows, what each block
reads from, and what was deliberately cut. Schema additions and the
recorder seam are sketched here; implementation lands in a follow-up
doc once the slice 1 PR opens.

---

## 1. The four questions

A prompt-tuning loop is the same four questions, in order, every time:

1. **Did my last change make it more expensive?**
2. **Did it make it slower?**
3. **Did it break anything?**
4. **What did the agent actually think / call?**

Every block on this tab earns its place by giving a direct answer to
one of those questions. Anything that does not is cut.

The hidden fifth question that ties the first four together:
**"compared to what?"** Without an anchor for "before the change vs
after the change", every number on the page is decoration. Section 4
makes the anchor a first-class part of the schema.

---

## 2. What already exists

| Source | What it gives us |
|---|---|
| `prompt_requests` | per-turn row: status, attempts, failure_reason, cancellation, kind (normal / reflection / resolution), sender → receiver, DAG root |
| `prompt_request_dags` | DAG-wide turn budget (`turns_used` vs `turns_cap`) |
| `prompt_response_chunks` | every streamed chunk verbatim, including `ResponseChunk::Reasoning` |
| `session_messages` (JSONB body) | text + `AssistantContent::Reasoning` + tool calls / results |
| `tool_calls` | per-invocation: agent, mcp_server, tool_name, duration_ms, is_error, error_message — indexed for "per agent" and "per connection" |
| `memory_events` | every memory write, with `source_turn_id` provenance |

---

## 3. What is missing

1. **Token usage is not persisted anywhere the customer can read.**
   `Usage { input_tokens, output_tokens, cache_creation_input_tokens,
   cache_read_input_tokens }` is computed inside the provider call in
   [`src/provider/anthropic/client.rs`](../src/provider/anthropic/client.rs)
   and [`src/provider/openai/client.rs`](../src/provider/openai/client.rs),
   then discarded once the response leaves the function. No DB row
   carries it.
2. **`prompt_requests` has no per-turn metrics.** No `duration_ms`, no
   `model_used`, no `provider`, no token columns. The audit row tells
   the customer the turn succeeded — not what it cost.
3. **No prompt-version history.** When the customer edits
   `agents.system_prompt` or `agents.model`, the previous value is
   overwritten in place. There is no row that says "from 14:00 onward
   the agent ran on this prompt." Without this, before/after
   comparison is impossible.
4. **No frontend.** `web/src/pages/` has General / Tools / Memory but
   no logs page; the nav item is `disabled: true` in
   [`AgentLayout.tsx`](../web/src/components/templates/AgentLayout.tsx).

There is **no budget feature today**. `prompt_request_dags.turns_cap`
caps DAG hop count, not tokens, not per-agent, not monthly. This tab
does not pretend otherwise — see §8.

---

## 4. Schema — shaped to answer the four questions directly

Two new tables. The second is what makes "compared to what?" answerable.

### 4.1 `agent_prompt_versions` — the answer anchor

Every edit to an agent's system prompt or model produces a new row.
The agents table keeps the current values (cheap reads for the worker,
no JOIN per turn); this table is the append-only audit history.

```sql
-- migration 43: prompt + model version history per agent
CREATE TABLE agent_prompt_versions (
    id            UUID PRIMARY KEY,
    agent_id      UUID NOT NULL REFERENCES agents(id) ON DELETE CASCADE,
    org_id        UUID NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    version       INTEGER NOT NULL CHECK (version > 0),  -- monotonic per agent
    system_prompt TEXT NOT NULL,
    model         TEXT,                                  -- NULL = workspace default
    edited_by     UUID,                                  -- user_id, NULL = system seed
    created_at    TIMESTAMPTZ NOT NULL,
    UNIQUE (agent_id, version)
);
CREATE INDEX agent_prompt_versions_agent_idx
    ON agent_prompt_versions (agent_id, created_at DESC);
-- + RLS + enforce_org trigger mirroring migration 25.
```

A bump is triggered by **any** change to `system_prompt` or `model`
via `PATCH /agents/:id`. Treating model and prompt as one tuple is
deliberate: from a tuning-loop perspective, a model swap and a prompt
edit are the same act (the agent's behaviour changed at time T).

Migration seeds `version = 1` for every existing agent.

### 4.2 `turn_metrics` — one row per LLM call

```sql
-- migration 44: per-turn metrics (normal, reflection, and resolution turns)
CREATE TABLE turn_metrics (
    request_id            UUID PRIMARY KEY REFERENCES prompt_requests(id) ON DELETE CASCADE,
    org_id                UUID NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    session_id            UUID NOT NULL REFERENCES sessions(id)       ON DELETE CASCADE,
    agent_id              UUID NOT NULL REFERENCES agents(id),
    prompt_version_id     UUID NOT NULL REFERENCES agent_prompt_versions(id),
    kind                  TEXT NOT NULL,       -- mirrors prompt_requests.kind
    model                 TEXT NOT NULL CHECK (octet_length(model) BETWEEN 1 AND 128),
    provider              TEXT NOT NULL CHECK (provider IN ('anthropic','openai')),
    input_tokens          INTEGER NOT NULL CHECK (input_tokens  >= 0),
    output_tokens         INTEGER NOT NULL CHECK (output_tokens >= 0),
    cache_creation_tokens INTEGER,             -- nullable: provider may omit
    cache_read_tokens     INTEGER,
    duration_ms           INTEGER NOT NULL CHECK (duration_ms >= 0),
    stop_reason           TEXT NOT NULL,       -- end_turn|tool_use|length|other
    started_at            TIMESTAMPTZ NOT NULL,
    created_at            TIMESTAMPTZ NOT NULL
);
CREATE INDEX turn_metrics_agent_idx           ON turn_metrics (agent_id, started_at DESC);
CREATE INDEX turn_metrics_session_idx         ON turn_metrics (session_id, started_at DESC);
CREATE INDEX turn_metrics_agent_version_idx   ON turn_metrics (agent_id, prompt_version_id);
-- + enforce_org trigger + RLS, mirroring migration 25.
```

Why the columns map to the questions one-for-one:

| Question | Columns that answer it |
|---|---|
| Q1 expense | `input_tokens + output_tokens`, `cache_read_tokens`, grouped by `started_at` bucket and/or `prompt_version_id` |
| Q2 latency | `duration_ms`, grouped or as `percentile_cont(0.5/0.95)` |
| Q3 broken | `stop_reason`, joined to `prompt_requests.status` and `failure_reason` |
| Q4 reasoning / tools | `request_id` is the foreign key into `tool_calls`, `memory_events.source_turn_id`, and `session_messages` reasoning blocks. The drawer joins these. |
| "compared to what?" | `prompt_version_id` slices every column above by version |

### 4.3 `hook_events` (optional, slice 3)

```sql
CREATE TABLE hook_events (
    id         UUID PRIMARY KEY,
    org_id     UUID NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    session_id UUID NOT NULL REFERENCES sessions(id)      ON DELETE CASCADE,
    request_id UUID NOT NULL REFERENCES prompt_requests(id) ON DELETE CASCADE,
    agent_id   UUID NOT NULL REFERENCES agents(id),
    phase      TEXT NOT NULL CHECK (phase IN ('before_turn','after_turn','before_tool','after_tool')),
    hook_name  TEXT NOT NULL CHECK (octet_length(hook_name) BETWEEN 1 AND 128),
    decision   TEXT NOT NULL CHECK (decision IN ('continue','deny')),
    reason     TEXT,
    tool_name  TEXT,
    created_at TIMESTAMPTZ NOT NULL
);
```

**`hook_name` carries its arguments inline.** A hook that takes a
target (e.g. an allowlist check against a specific MCP server) writes
the rendered string into `hook_name` directly — `allowlist.check(security-mcp)`,
not a separate `target` column. The 128-byte cap fits any realistic
hook signature; the recorder truncates if longer. This keeps the
schema flat and the read path one-column, at the cost of slightly
fuzzy grouping if the same hook is invoked with many distinct
arguments (acceptable — denials are rare and the UI groups by
exact-match anyway).

Customer-facing only as a per-turn detail in the drawer. Not a
dashboard panel (see §7).

### 4.4 Recorder seams

- **Prompt version recorder.** `PATCH /agents/:id` (and the initial
  agent seed path) computes `next_version = max(version) + 1` for the
  agent, inserts an `agent_prompt_versions` row inside the same
  transaction as the UPDATE on `agents`. No bump if neither
  `system_prompt` nor `model` actually changed.
- **Turn recorder.** `agent_core::turn::call_provider` already returns
  `ChatResponse { usage, model, stop_reason, … }`. Add a
  `TurnRecorder` mirroring [`ToolCallRecorder`](../src/tools/pg_recorder.rs)
  — same shape, in-memory fake for tests — and invoke it immediately
  after the provider returns. The current `prompt_version_id` is
  resolved once at the start of `call_provider` and threaded through.
  Reflection and resolution turns flow through the same function, so
  they record without a second wiring site. One INSERT per turn,
  ~80 bytes per row.

### 4.5 Restoring a previous prompt version

```
POST /agents/:id/prompt-versions/:version/restore
```

The diff modal's **"Apply v6"** button (slice 4) calls this endpoint.
Mechanism, in one transaction:

1. `SELECT system_prompt, model FROM agent_prompt_versions
    WHERE agent_id = :id AND version = :version`
2. `next_version = max(version) + 1` for this agent
3. `INSERT INTO agent_prompt_versions` with the snapshot from step 1,
   `version = next_version`, `edited_by = current_user_id`,
   `created_at = now()`
4. `UPDATE agents SET system_prompt = …, model = … WHERE id = :id`

Reverting v7 → v6 produces a new **v8** whose content is byte-identical
to v6 but whose id, timestamp, and editor are new. The history table
is never re-pointed and never rewritten — append-only. Turn metrics
that ran on v6, v7, and v8 each remain pinned to their own
`prompt_version_id`, so the "compared to what?" lens stays accurate
across reverts.

---

## 5. The design — one chart, one table, one drawer

```
┌─────────────────────────────────────────────────────────────────────┐
│  Last 24h ▾    Kind: all ▾    Compare: prev 24h ▾    updated 12s ago│
├─────────────────────────────────────────────────────────────────────┤
│                                                                     │
│   TOKEN SPEND (stacked by kind)             4.2M  ▲ 12% vs prev 24h │
│   ▓▓▓░░ ▓▓▓▓░ ▓▓▓▓▓ ▓▓░░░ ▓▓▓▓▓ ▓▓▓▓░ ▓▓▓░░ ▓▓▓▓▓                  │
│                  ↑ v7 edited                                        │
│   p50 3.2s  ▲ p95 9.1s  ▲ from 6.2s     ✗ 3 failed  ▲ from 0        │
│                                                                     │
├─────────────────────────────────────────────────────────────────────┤
│  TURNS                                                              │
│                                                                     │
│  TIME      PROMPT  KIND        MODEL              TOKENS  LATENCY  OUTCOME│
│  04:12:33  v7      normal      claude-opus-4.7    8,754   3.4s     ✓     ▸│
│  04:11:50  v7      reflection  claude-haiku-4    12,282   1.1s     ✓     ▸│
│  04:09:22  v7      normal      claude-opus-4.7    9,001  28.0s     ✗ tmo ▸│
│  ───────── prompt edited by alice@acme.io · v6 → v7 ──────────────────────│
│  03:58:14  v6      normal      claude-opus-4.7    6,290   2.9s     ✓     ▸│
│  …                                                                  │
└─────────────────────────────────────────────────────────────────────┘
```

That is the entire tab.

### 5.1 Scope strip

Three controls. Anything else is overhead.

- **Time range** — `1h / 24h / 7d / 30d / custom`. Default 24h.
- **Kind filter** — `normal / reflection / resolution / all`. Default
  `all`. The one filter that materially changes the picture: reflection
  turns can dominate token totals and silently confuse anyone tuning
  the user-facing system prompt.
- **Compare** — `prev equal window / none / specific version vs current`.
  Default `prev equal window`. This is what makes every number on the
  page answer "did my change help?" directly.

Right-aligned: `updated 12s ago` mono caption. The data is read from
Postgres rollups, not realtime. 10–30 s lag is expected and called
out so customers don't refresh expecting live behaviour.

### 5.2 Token spend chart

One `SectionCard`, eyebrow `TOKEN SPEND`. The only over-time view in
the tab.

- X axis: time bucket (auto: 5 min / 1 h / 1 d based on window).
- Y axis: tokens.
- Bars stacked by `kind`. No segment toggles, no "by model", no
  in/out/cache split — the toggles were dead UI in practice and the
  caption already covers the headline numbers.
- **Prompt-edit markers.** Vertical dashed line at every
  `agent_prompt_versions.created_at` that falls inside the window,
  labelled `↑ v7 edited`. Click jumps the timeline to the first turn
  after that edit. This is what lets a customer see "tokens jumped
  right after I changed the prompt" without reading numbers.
- **Caption row, mono, three groups separated by `·`:**
  - `{total tokens} · {Δ vs compare window}` → answers Q1
  - `p50 {ms} · p95 {ms} · {Δ p95 vs compare window}` → answers Q2
  - `✗ {failed count} · {Δ vs compare window}` → answers Q3
- Hover tooltip on a bar: bucket start, total, breakdown by kind,
  turn count, and the prompt version active for that bucket.
- Click a bar → filters the timeline below to that bucket.

P50 and P95 instead of average. Averages lie when one 28-second
timeout drags the mean.

Renders as a flat `<svg>` with `<rect>`s. No charting library
dependency — both for visual fit and CLAUDE.md §8 (zero-dependency
bias) rules out pulling one in for this.

### 5.3 Turns timeline

One `SectionCard`, eyebrow `TURNS`. Reuses the grid-row pattern from
[`AgentActivityCard`](../web/src/components/agentDetail/AgentActivityCard.tsx);
no new primitives.

One row per `prompt_requests` row in the window, joined to
`turn_metrics`.

Columns:
- **TIME** — start time, mono.
- **PROMPT** — `v7`, `v6`, … from `prompt_version_id`. Sortable.
  The whole point of this column is that the customer can sort by it
  and see every `v6` turn vs every `v7` turn next to each other.
- **KIND** — `normal` / `reflection` / `resolution`. Reflection rows
  get a subtle moss-tint background.
- **MODEL** — model id from `turn_metrics.model`.
- **TOKENS** — `in + out` sum. Hover tooltip: `in / out / cache_read / cache_creation`.
- **LATENCY** — `duration_ms`, formatted.
- **OUTCOME** — `stop_reason` on success; `failure_reason` on failure.
  Failed/timeout rows get rose-soft background.
- **▸** — expands the drawer in-place.

**Prompt-edit separators.** Whenever consecutive rows straddle an
`agent_prompt_versions` boundary, a single-line separator row appears
between them: `── prompt edited by alice@acme.io · v6 → v7 ──`. Pure
visual aid; no data attached.

Sortable on every column. No status filter in the scope strip — rose
rows are visible by eye, and a click on OUTCOME sorts them to the top.

Pagination: infinite scroll, `useInfiniteQuery`, page size 50.

### 5.4 Turn drawer

Click ▸ on any row. Renders in-place below the row, no nav.

1. **Reasoning** — pulled from `session_messages.body` where the
   assistant content has `AssistantContent::Reasoning(...)`. Collapsed
   by default with byte count. This is the direct answer to Q4.
2. **Tool calls** — filtered `tool_calls.request_id = row.id`. Reuses
   the existing row component.
3. **Memory writes** — filtered `memory_events.source_turn_id = row.id`.
   Summary line `+3 written · 1 forgotten`, click-to-expand list.
   Critical for tuning: lets customers see "the prompt made the agent
   over-write to memory" without grepping logs.
4. **Prompt used for this turn** — `<details>` block showing the
   exact `agent_prompt_versions` row (system_prompt + model). Read-only.
   This is the direct answer to "what was the agent running when this
   turn happened?" — sometimes the regression is obvious once you see
   the prompt next to the broken output.
5. **Hook events** — filtered `hook_events.request_id = row.id`. Only
   if/when `hook_events` ships (§9 slice 3).

No raw request/response payload dump. Customers see what they put in
(prompt, model) and what came out (reasoning, tool calls, memory
writes). The raw provider wire format is an internal detail.

---

## 6. API surface

Three endpoints, all org-scoped via the existing
`app_user_is_member` RLS. Aggregations live in SQL — never ship raw
`turn_metrics` rows to power the chart.

```
GET /agents/:id/metrics/timeseries?from&to&bucket&compare
    → chart + caption + prompt-edit markers (one round-trip)

GET /agents/:id/turns?from&to&kind&cursor
    → timeline rows (joined to turn_metrics, includes prompt_version label)

GET /turns/:request_id
    → drawer detail (joins turn_metrics + tool_calls + memory_events
      + session_messages reasoning + agent_prompt_versions snapshot)

POST /agents/:id/prompt-versions/:version/restore
    → revert agent to a prior version's prompt + model snapshot
      (mechanism in §4.5; produces a new version row, never rewrites)
```

`/metrics/timeseries` returns the whole answer payload in one shape:
buckets, per-bucket totals by kind, latency percentiles, failure count,
delta vs compare window, and the list of `agent_prompt_versions` edits
in the window. The frontend never recomputes deltas client-side.

`/turns/:request_id` is the only endpoint that fans out across tables.
Bound it with `MAX_TOOL_CALLS_PER_TURN` / `MAX_HOOKS_PER_TURN` caps
per CLAUDE.md §5.

---

## 7. What was cut, and why

The first draft of this design had four KPI tiles, a side-by-side
tool-calls panel, a side-by-side hook-decisions panel, and a right-rail
spend summary. All cut.

| Cut | Why |
|---|---|
| KPI tiles (Turns, Tokens, Latency, Errors) | Total tokens and latency are already in the chart caption. Error count is already visible as rose-tinted rows in the timeline. Tile + chart + table footer would show the same number three times. |
| Side panel: tool calls | The only useful question — "what tools did *this turn* fire" — is answered by the drawer, in context. A flat tool-calls log duplicates the existing AgentActivityCard on the General tab. |
| Side panel: hook decisions | Hook denies are rare and the consequence already appears in the chat ("I can't do X"). Belongs in the drawer when scoped to a turn, not as a standing dashboard panel. |
| Right rail: spend summary | Every number in it was already on the chart or in the table footer. |
| Right rail: budget | Feature doesn't exist. `turns_cap` caps DAG hops, not tokens; no per-agent / monthly budget anywhere in the schema. Showing a "4.2M / 10M" denominator would invent UI for a feature that isn't built. |
| Status filter | Failed rows are rose-tinted. The eye does this for free. |
| Model filter | `agents.model` is a single column today; the dropdown would sit empty 99% of the time. Sort the MODEL column instead if comparing. |
| Chart segment toggles (by model / in-out-cache split) | Interesting once, never toggled again. Dead UI. |
| Avg latency anywhere | Outliers (one 28 s timeout turn) drag the mean and lie. P50 + P95 in the caption instead. |
| USD cost numbers | Requires a versioned per-model price table. A wrong dollar number is worse than no number. Add when there is a real price source. |
| Raw request/response JSON in the drawer | Internal-only detail, not customer-meaningful. Show the *prompt version* used instead — same answer, customer language. |
| Live tail | The data is audit-flavoured. 10–30 s lag is fine and called out in the scope strip. Doubling the websocket plumbing for marginal value is not worth it. |

---

## 8. What this tab is *not*

- Not realtime. 10–30 s lag is expected and called out.
- Not a billing dashboard. No prices, no caps, no enforcement.
- Not the chat audit log. Per-message conversation history lives in
  the chat view; this tab is per-turn observability for the customer,
  not per-message review.
- Not an internal debugging surface. No request/response wire dumps,
  no external observability links, no infrastructure-level fields. If
  the engineering team needs deeper diagnostics, that lives in a
  separate internal-only tool.

---

## 9. Slicing

**Slice 1 — the whole MVP.** Adds `agent_prompt_versions` +
`turn_metrics` migrations + the prompt-version bump in `PATCH /agents/:id`
+ the turn recorder + the two API endpoints + scope strip + chart +
timeline table. Removes `disabled: true` from `AgentLayout`. No
drawer yet. ~4–5 day's work.

**Slice 2 — the drawer.** All data exists once slice 1 lands; no new
tables. Reasoning + tool calls + memory writes + prompt-used block.
~1–2 days.

**Slice 3 — hook audit.** `hook_events` table + recorder + drawer
section. Optional; the four questions still get answered without it.
~1 day.

**Slice 4 — version compare view.** Side-by-side `v6 vs v7`: same
chart and timeline, two columns. Pure UI on top of the slice-1 data
shape. ~1–2 days.

**Slice 5 — budget.** Out of scope. Whenever budget ships as a real
feature, the chart caption gains a `/ {cap}` suffix and the timeline
gains a denial row type; no new tab section needed.
