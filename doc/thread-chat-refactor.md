# Thread-chat refactor — plan + handoff

> **Status (handoff):** on branch `feat/thread-chat-refactor`, all work **uncommitted**.
> Phases **P0–P3 cores are done and verified green** against local Postgres; the dev DB is
> migrated to `63`. The remaining cluster (P4–P11) is described below with the gaps discovered
> while building P0–P3. Read §6 (Progress) and §7 (Discovered gaps) first when resuming.

---

## 1. Context

Patom's chat only works for a 1-human ↔ 1-agent org. A "session" is a 2-party *pair*
(`sessions.participant_a/b_colleague_id`), a "thread" is conflated with a DAG (`root_request_id`),
and an agent's LLM context is a viewer-mapped snapshot of that pair's `session_messages`. Real
workspaces have many humans + many agents in one conversation; the pair model fragments an agent's
view across N pair-sessions and can't represent "many participants in one thread."

**Goal:** make an agent a first-class chat participant. org → **channel** → **thread** (Slack-exact,
two message layers) with one canonical feed per thread. An agent's "session" becomes
**`(thread_id, agent_id)`** participation; everyone else is its counterparty. `send_message` is the
agent's sole output verb. The existing **claim-and-drain** machinery, re-keyed to `(thread, agent)`,
gives coalesce + per-agent serialize + re-address-follow-up for free.

Ships as **ONE PR** (consistent view; breaking downstream during the change is accepted). Full DB
**reset** is approved (friends-only prod) — one migration drops the old model and creates the new,
no backfill.

## 2. Locked design decisions

- **session ≔ `(thread_id, agent_id)`**; the pair model is deleted.
- **Slack-exact, two message layers**: channel timeline + threaded replies. **Three trigger
  sources:** (1) human @tags an agent in a channel message → auto-create a thread rooted at that
  message; (2) a **scheduled task fires** → the agent *initiates* a thread in the task's target
  channel; (3) an agent `send_message`s another agent → trigger in the same thread.
- Agent **reasoning/tool-calls stay in-thread** (never clutter the timeline), but an agent's
  **posted** messages live in threads whose root surfaces in the channel timeline like any thread
  root (e.g. a scheduled 7am summary in `#abc` tagging a user).
- **One feed, one per-thread ordering authority.** Agent reasoning/tool-calls are **shown to
  everyone** (transparency) but **not ingested into other agents' LLM context** (peers see only
  posted messages).
- **`send_message` is the SOLE output verb**: receiver=agent → post + trigger + debit budget;
  receiver=human → post + notify (**human must be a channel member, else reject — no auto-add**);
  empty → post untagged. **Agents are org-global** (reachable in any channel, no membership); only
  humans are channel-gated.
- **read-at-run** for chat turns (agent reads the thread tail when the worker claims it).
- **Budget per (human-tag / scheduled fire, agent)**: each root trigger mints a fresh
  `root_request_id`/DAG, `turns_cap = MAX_DAG_TURNS = 64`. `@X @Y` in one message = two DAGs.
- **Force-exit preserved**: worker `run_with_pingpong_guard` (egress now = a posted feed row
  landed); background cognition skips it.
- **RLS by org + channel membership**, not the single `created_by_user_id`→`app.user_id` pin.
- **Reflection/resolution = private background cognition**, rehomed off pair-sessions to a private
  `background_turns` store; **no `Participant::System` rows in the chat feed**.
- Reuse the existing claim-and-drain, re-keyed to a polymorphic
  **`claim_key = COALESCE(state_id, background_turn_id)`**.

## 3. Target data model (migration `00000000000063_thread_feed.{up,down}.sql` — already written)

New tables: `threads`, `thread_messages` (one feed; `kind ∈ posted|reasoning|tool_use|tool_result|
system_note`; `owner_agent_id` for private artifacts; `surface id UNIQUE` so a channel message can
root a reply-thread), `thread_seq`, `agent_thread_state` (the redefined session), `background_turns`
+ `background_turn_messages` (cognition rehome), `claim_leases` + `claim_seq` (polymorphic
`claim_key`, no FK — ephemeral). `prompt_requests` becomes a **trigger row**: adds
`thread_id`/`state_id`/`background_turn_id`/`trigger_message_id`/`acting_user_id` + a
`state_id XOR background_turn_id` CHECK, `content` made nullable, `session_id` dropped, pending index
re-keyed to `COALESCE(state_id, background_turn_id)`. `scheduled_tasks` gains `channel_id`.
FK-dependents repointed off `sessions` (see §7). Drops `sessions`/`session_messages`/
`session_turn_seq`/`session_leases`.

The `.up` migration starts with `DELETE FROM prompt_requests;` (cascades clear runtime rows;
`agent_memories.source_turn_id` is SET NULL so memories survive). The `.down` is **lossy** (recreates
the dropped tables' structure + RLS but not their org-parity triggers; no data) — acceptable per the
reset waiver.

## 4. Build order (one PR; each phase opens with a failing test, CLAUDE.md §3)

| Phase | Work | Opening test |
|---|---|---|
| **P0** | Migration 63 | applies + `.down` reverses |
| **P1** | `PgThreadStore` (`src/threads/`) | `context_filters_private_rows_by_owner` |
| **P2** | Queue re-key to `claim_key` | `claim_serializes_per_thread_agent` + coalesce |
| **P3** | Budget mint per root trigger | `two_tags_one_message_mint_two_dags` |
| **P4** | Agent loop read-at-run | `context_is_read_at_run` |
| **P5** | `send_message` sole verb | `send_to_human_non_member_rejected_no_autoadd` |
| **P6** | Worker force-exit + principal | `no_egress_nudges_then_fails` (egress = posted row) |
| **P7** | RLS membership scoping | `channel_feed_scoped_to_membership_not_active_user` |
| **P8** | Reflection/resolution rehome | `reflection_writes_no_thread_message_rows` |
| **P9** | Scheduling third trigger | `fire_creates_thread_and_agent_posts_summary_tagging_owner` |
| **P10** | Slack + HTTP + stream re-key | `g2_canonical_feed_seq_order_multi_party`; stream-by-thread |
| **P11** | Web FE + dead-code sweep | preview verify; all gates green |

(Full per-module change list for P4–P11 is in the approved plan; condensed pointers in §8.)

## 5. Architecture approach: ADDITIVE

The plan said "rewrite `session/` in place," but P1/P2 were built **additively** (new `threads/`
module + new queue methods alongside the old session-keyed path) so each phase stays independently
compilable and verifiable with a real green test. **Same one-PR end state** — the old `session/`
module, old queue methods, and the old `sessions`-keyed tables/tests are deleted in **P11**. The old
session code still *compiles* (runtime SQL per §10) but its tests are **red** because P0 dropped the
tables it queries — that redness is expected until P11.

Naming: did **not** repurpose `SessionId`. Introduced fresh newtypes `ThreadId`, `AgentThreadId`
(= `agent_thread_state.id`, the chat `claim_key`), `ThreadMessageId`. `Participant` + `MessageSender`
(incl. `System`) are kept; `Participant::canonical_pair`/`canonical_cmp` get deleted in P11.

## 6. Progress so far (verified green)

**P0 ✅** — `migrations/00000000000063_thread_feed.{up,down}.sql`. Verified via
`sqlx migrate run` → `revert` → `run` (up/down/up clean). Dev DB is at `63`.

**P1 core ✅** — `src/threads/{mod,error,traits,pg_store}.rs`, wired in `lib.rs`. Implements
`create_thread`, `resolve_participation`, `append` (atomic `thread_seq` + `last_activity_at` bump),
`context_for_agent` (posted ∪ own-private, viewer-mapped). Test `tests/pg_thread_store.rs::
context_filters_private_rows_by_owner` green.
**Remaining in P1** (additive, land with consumers): `append_turn_block` (tool_use/result consecutive
seqs — P4), `feed_tail` + `participants` (HTTP — P10), author-name labeling on others' posted rows
(roster — P8/P10; there's a `TODO(P8/P10)` in `pg_store.rs map_row_for_viewer`), and deleting the
pair methods (P11).

**P2 core ✅** — `runtime/queue.rs` (`NewTrigger`, `ClaimedTurn`), `runtime/pg_queue.rs`
(`enqueue_trigger`, `claim_next_turn` + helpers `next_turn_candidate`/`bump_claim_seq`/
`try_take_claim_lease`/`drain_turn_pending`/`build_claimed_turn`), re-exported in `runtime/mod.rs`.
Tests `tests/pg_turn_queue.rs` (coalesce + per-(thread,agent) serialize, concurrent across agents)
green. `lease_seq` uses the existing `TurnSeq` newtype.
**Remaining in P2:** the **background** claim path (only the chat/`state_id` path is built; background
triggers will exist after P8), and `mark_done`/release for the new path + the worker wiring (P6).

**P3 core ✅** — folded into `enqueue_trigger`: `NewTrigger.root_request_id: Option` — `None` ⇒ root
mint (anchor on own id + seed `prompt_request_dags` @ cap 64); `Some` ⇒ inherited. Test
`tests/turn_dag_mint.rs::two_tags_one_message_mint_two_dags` green.
**Remaining in P3:** the **debit** side (agent→agent bumping `turns_used` via `DagBudget::bump_or_fail`)
wires in when `send_message` is rewritten (P5).

A `/simplify` pass ran on P1–P3 code: only change applied was `i64`→`TurnSeq` for the lease seq.

**P4 core ✅** — `agent_core/{builder,core,turn,error,outcome}.rs` + `memory/{traits,static,agent,loader}.rs`.
`AgentBuilder::with_thread_store` + `Agent::reply_in_thread(claim_key, thread, viewer, …)` (no `prompts`
arg — read-at-run). `build_thread_request` reads `ThreadStore::context_for_agent`; the assistant turn +
tool results are appended as **owner-private** feed rows (`Reasoning`/`ToolUse`/`ToolResult`, owner =
agent). New `Memory::system_prompt_for_thread` (no session / no counterpart; multi-party feed) backed by
`MemorySectionLoader::load_stable` (stable layer only — contextual retrieval degrades empty until rehomed).
Hook/tracing/tool contexts still speak `SessionId`, bridged via `SessionId::from(claim_key.as_uuid())`
(= `turn_metrics.state_id`/`tool_calls.state_id`). `AgentError::Thread` variant added. Test
`tests/agent_thread_loop.rs::context_is_read_at_run` green; lib clippy `-D warnings` + fmt clean.
**Remaining in P4 / deferred:** the `tool_use`/`tool_result` re-pair at context-build (note 13) is still a
TODO; `todos` block + `turn_metrics`/`tool_calls` recorders are **not** wired into the thread path yet
(skipped — recorders still `INSERT session_id`; retype + wire in P6). `ToolCallContext` keeps `session_id`
(bridged) until P5 rehomes the session-coupled system tools onto `state_id`/`thread_id`.

**P5 core ✅** — `send_message` is now the thread-feed egress.
- `ToolCallContext` gains `thread_id`/`state_id` (legacy path = `None`; thread loop sets them, threading
  `claim_key` through `run_thread_turn`/`run_thread_tool_calls` — no UUID round-trip). `session_id` stays
  (bridged) for the still-session-coupled tools (get_session/scheduling/memory/todos — rehomed in their phases).
- `ThreadStore::is_channel_member` (human gate) + `ThreadStore::append` now returns `ThreadMessageId`.
- `PromptQueue` trait gains `enqueue_trigger` + `claim_next_turn` (so `SharedPromptQueue` reaches them; the
  trait methods delegate to the inherent impls via `Self::` — inherent shadows trait, no recursion; P11
  collapses to single defs).
- `tools/system/send_message.rs` rewritten: drops sessions/`context_summary`/`resolve_or_create_for_pair`,
  takes `SharedThreadStore`. `receiver` is `Option` (None ⇒ untagged post). Always posts a `kind='posted'`
  row at `ctx.thread_id` (the egress). Human → `is_channel_member` gate (reject `InvalidInput`, no auto-add,
  no post); Agent → post + `resolve_participation` + `bump_dag_budget` + `enqueue_trigger` (root inherited,
  `trigger_message_id` = posted row, idempotency `tag:{thread}:{agent}:{message}`).
- Wired: `app.rs` (constructs `PgThreadStore` into `BuiltinToolDeps`) + `tests/common/harness.rs` +
  `tests/runtime_pipeline.rs`. Deleted obsolete pair tests `{send_message_colleague,agent_to_agent,human_delivery}.rs`.
- Test `tests/agent_thread_send_message.rs::send_to_human_non_member_rejected_no_autoadd` green; full
  `cargo check --tests` compiles; lib+tests clippy `-D warnings` + fmt clean.
**Remaining in P5 / deferred:** the human-member happy path, agent-delivery, and untagged-post branches have
no dedicated tests yet (only the reject case); the SSE notify on human delivery is dropped until the P10
stream re-key (posting to the feed is the durable delivery). `ctx.root_request_id` in the thread loop is
still `= request_id`, not the resolved DAG root — wire the real root in P6 (worker → `ClaimedTurn`).

## 7. Discovered gaps / handoff notes (read before resuming)

These are facts learned while building P0–P3 that aren't obvious from the plan:

1. **`thread_messages.request_id` is nullable.** Plain human posts have no producing turn; only
   agent-produced rows carry it. (Plan implied NOT NULL.)
2. **Dropped `prompt_requests_enqueue_org` trigger.** Dropping `prompt_requests.session_id` required
   dropping the old `prompt_requests_enforce_org` parity trigger + pending index first. RLS still
   gates org membership; **a thread/state-scoped parity trigger should be re-added** when the queue
   is finalized (P2/P6). Not yet done.
3. **`agents` no longer has `system_prompt`** (moved to `agent_prompt_versions`). A raw test insert
   needs only `(id, name, is_default, created_at, updated_at, description, org_id)`; the
   `agents_mint_colleague` trigger mints the colleague.
4. **`tests/common/pg.rs::seed_prompt_request` is now BROKEN** — it inserts the old `session_id`
   column and sets neither `state_id` nor `background_turn_id` (violates the XOR CHECK). Fix or
   replace it before any phase uses it. (P1–P3 tests avoid it by appending with `request_id = None`.)
5. **Tests query the DB via `&pool` as the `patom` superuser, which BYPASSES RLS** — that's why
   direct `SELECT … FROM prompt_request_dags` works in tests despite FORCE RLS. App code must use
   `run_as_user` / `run_privileged`.
6. **FK-repointing done in migration 63** (prerequisite before dropping `sessions`):
   `session_todos` PK → `state_id`; `turn_metrics.session_id` → `state_id` (+ `turn_metrics_state_idx`,
   rewritten `enforce_turn_metrics_org`); `tool_calls.session_id` → `state_id` (rewritten
   `enforce_tool_calls_org`); `reflection_checkpoints` recreated keyed `(agent_id, thread_id)` with
   `last_message_id` (was `last_turn_id`) and **no `reflection_session_ids` array**. The Rust code
   that writes these tables (turn metrics recorder, tool-call recorder, todos store, reflection
   checkpoint writer) is **not yet updated** and will break compile until P4/P6/P8 — expected.
7. **`claim_key` is a polymorphic bare `Uuid`** (= `state_id` XOR `background_turn_id`), no FK. The
   right §1 shape is a `ClaimKey` sum type (`Chat(AgentThreadId)` / `Background(BackgroundTurnId)`);
   **deferred to P8** when the `Background` variant + a `BackgroundTurnId` newtype become real. Until
   then `NewTrigger.background_turn_id: Option<Uuid>` is unused (always `None`).
8. **My `claim_next_turn` already returns `receiver_colleague_id` from the drain `RETURNING`** — it
   does NOT make the extra colleague lookup that the old `claim_next_session` does. Don't reintroduce
   it when wiring the worker.
9. **`enqueue_trigger` resolves the receiver colleague before the idempotency `ON CONFLICT`** — this
   is intentional (optimizes the common new-insert path); don't "fix" it to check idempotency first.
10. **Stream/quiescence (R1, P10) is the biggest unaddressed coupling.** SSE + `pg_thread_stream` +
    worker quiescence are keyed on `root_request_id`, but one thread now hosts **many** DAGs (one per
    trigger). Re-key to `thread_id`; redefine terminal as "no live lease on any `(thread, *)`";
    `ResponseChunk::AgentMessage.to_session` → `thread_id`.
11. **`acting_user_id` is denormalized onto `prompt_requests`** (R2) so the claim is a single join
    (no `sessions` lookup); it flows to the worker's RLS principal + `ClaimReceipt`.
12. **Channel-level @tag routing (R3, P5):** a timeline @tag auto-creates a thread rooted at that
    channel message; the agent works there. Pin + test this rule.
13. **tool_use/tool_result adjacency (R4, P1/P4):** threads are multi-writer, so a peer's posted row
    could land between an agent's tool_use and tool_result by `seq`. Guard: write the pair at
    consecutive seqs in one tx (`append_turn_block`) **and** re-pair owner-private rows by
    `(request_id, owner_agent_id)` at context-build before the provider call. `context_for_agent`
    does **not** yet do the re-pair (TODO in P4).
14. **Idempotency keys (R6):** human-tag trigger `tag:{thread_message_id}:{agent_id}`; agent→agent
    `tag:{thread_id}:{agent_id}:{trigger_message_id}`; scheduled keeps `sched-{task_id}-{fire_ts}`.
15. **Scheduled-target membership (R9):** a scheduled agent's tagged user must be a channel member
    (send_message human-gate), else the post rejects.

## 8. Remaining phases — condensed pointers (full detail in the approved plan)

- **P4 agent loop** (`agent_core/{core,turn}.rs`): `reply(claim_key, thread_id, viewer)` drops the
  `prompts` arg (read-at-run); `build_chat_request` → `threads.context_for_agent` (prepend the
  thread's root channel-message); append assistant/reasoning/tool rows to `thread_messages`; delete
  `snapshot`/`parent_history_for_viewer` calls. Needs a mock-LLM harness to drive a turn end-to-end.
- **P5 `send_message`** (`tools/system/send_message.rs`): receiver agent(always)/human(channel-member
  gated, no auto-add)/empty; always post `kind='posted'` (the egress); agent → `resolve_participation`
  + `bump_dag_budget` + `enqueue_trigger` (root inherited); delete the pair side-session +
  `context_summary`.
- **P6 worker** (`runtime/worker.rs`): switch claim from `claim_next_session` → `claim_next_turn`;
  consume `ClaimedTurn` (`trigger_ids`, `thread_id`, `acting_user_id`); keep `run_with_pingpong_guard`
  verbatim (egress = posted row; nudge = `system_note`); add `mark_done`/release for the claim_key
  path; RLS principal `Caller::new(claim.acting_user_id, claim.org_id)`.
- **P7 RLS:** member-scoping in the query layer (channel_members + not archived; DM = channel-less +
  participant); remove the single-user pin / DM-ownership predicate; agents org-global.
- **P8 reflection/resolution:** rehome onto `background_turns`/`background_turn_messages` (no chat-feed
  rows); `find_candidates` scans `agent_thread_state` + `thread_messages`; `RequestKindPayload::
  Reflection{thread_id, up_to_message_id}` (frozen slice, NOT read-at-run); checkpoint
  `(agent_id, thread_id)`; build the **background claim path** + introduce **`ClaimKey`/
  `BackgroundTurnId`** here (note 7).
- **P9 scheduling:** `scheduled_tasks.channel_id`; `ScheduledTaskScheduler.fire` → `create_thread`
  in the channel + seed an owner-private `system_note` instruction + `resolve_participation` + mint
  DAG (acting_user=owner) + `enqueue_trigger` (Normal). Tool/route gain `channel_id`.
- **P10 HTTP + Slack + stream:** rewrite G1/G2 (flat feed, per-author label+avatar for everyone,
  expose `kind`, drop pair `receiver`); G3 stream re-key to `thread_id` (note 10); `slack_threads`
  bind to `thread_id`; `POST /prompts` @tag routing; `get_session` → read thread tail.
- **P11 web FE + sweep:** `web/src/{types/api,lib/api,stores/threadStore,hooks/useThreadStream,
  pages/ChatView,lib/demo}.ts` + `web/mock-backend.ts`; **delete the old `session/` module + old
  queue methods + pair tests + `canonical_pair`**; all gates green (fmt, clippy `-D warnings`,
  check, nextest, deny/audit).

## 9. Verification / how to run

- Local PG: `docker compose up postgres` (DATABASE_URL in `.env`).
- Per-phase test: `set -a && . ./.env && set +a; cargo test -p patom-core --test <name>`.
- Migration round-trip: `sqlx migrate run --source crates/patom-core/migrations` then `… revert`.
- Final gates (P11): `cargo fmt --all -- --check`, `cargo clippy --all-targets --all-features
  -- -D warnings`, `cargo check --all-targets --all-features`, `cargo nextest run --all-features`.
- FE: extend `web/mock-backend.ts`, then preview tools (`preview_start` / `preview_screenshot`).

## 10. Key files

- Schema: `crates/patom-core/migrations/00000000000063_thread_feed.{up,down}.sql`
- New store: `crates/patom-core/src/threads/{mod,error,traits,pg_store}.rs`
- Queue: `crates/patom-core/src/runtime/{queue,pg_queue,dag}.rs`
- To rewrite next: `crates/patom-core/src/agent_core/{core,turn}.rs`,
  `crates/patom-core/src/runtime/worker.rs`, `crates/patom-core/src/tools/system/send_message.rs`
- Memory/scheduling/slack/http for later phases: `src/memory/*`, `src/scheduling/*`, `src/slack/*`,
  `src/http/routes/{threads,prompts,scheduling}.rs`, `src/runtime/{response,pg_thread_stream}.rs`
- Tests landed: `crates/patom-core/tests/{pg_thread_store,pg_turn_queue,turn_dag_mint}.rs`
- To fix: `crates/patom-core/tests/common/pg.rs` (`seed_prompt_request`, note 4)
