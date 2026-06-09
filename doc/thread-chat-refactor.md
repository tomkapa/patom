# Thread-chat refactor — plan + handoff

> **Status (handoff, updated through P11):** branch `feat/thread-chat-refactor`, dev DB migrated to `64`.
>
> **Done + verified green: P0–P11.** Commits this effort: `d077e6e` (P10 rehome — stream + slack + prompts
> onto `thread_id`), `26a7bfb` (P11 — delete the `session/` module + retype recorders onto `state_id`),
> `078a1e0` (P11 — advance reflection checkpoint on success). Lib `clippy --lib -D warnings` + `fmt` clean;
> `cargo check --all-targets` clean; the full `cargo test -p patom-core` suite green.
>
> **P10 rehome (`d077e6e`):** migration `64` (`slack_threads` + `mcp_oauth_pending` → `thread_id`);
> `ResponseChunk::AgentMessage{to_thread}`; `pg_response` PUBLISH_CTE reads/notifies `thread_id` (fixed the
> runtime-broken publish); `PgThreadStream` keyed by `thread_id`; G3 route on `thread_id` + `ThreadStore::
> visible_to`; `POST /prompts` + MCP-OAuth resume on create-thread/append/`enqueue_trigger`; slack
> `thread_map`/`bridge`/`stream_pump`/`connect_link` (one Patom thread ↔ one Slack thread); `send_message`
> re-publishes an `AgentMessage` egress chunk; `get_session` deleted; `ThreadStore` gained `visible_to` +
> `last_agent`.
>
> **P11 (`26a7bfb` + `078a1e0`):** deleted `src/session/` entirely + the legacy agent pair path
> (`reply`/`resume`/`run_loop`/`counterpart`/`run_turn`) + the old queue surface (`enqueue`/
> `claim_next_session`/`mark_done`/`mark_failed`/`LeaseManager`/`ClaimedSession`/`LeaseToken`/
> `NewPromptRequest`/`EnqueueOutcome`); introduced the `ClaimKey` newtype as the polymorphic turn scope
> (memory + hook contexts), retyped the `turn_metrics`/`tool_calls`/`session_todos` recorders + todos onto
> `state_id: AgentThreadId` (they FK `agent_thread_state`, so chat-only — skipped on the background path);
> wired the per-thread `<todos>` block into the thread context; rehomed the librarian **resolution** enqueue
> onto background turns; `RequestStatusView.session → claim_key`; `turns.rs` off the dropped tables; deleted
> ~17 pair-model / dropped-table test files. **Reflection checkpoint write** (`078a1e0`): the worker advances
> `reflection_checkpoints (agent, thread).last_message_id` on success so the scheduler stops re-enqueuing
> each idle window.
>
> **Remaining (deferred hardening + FE):** (1) the **web FE** rehome onto the new G1/G2/G3 wire +
> `mock-backend.ts` — in progress in this effort. (2) **tool_use/tool_result re-pair** at context-build
> (note 13) — a concurrent-interleaving edge case; `context_for_agent` does not yet reorder a tool_use/
> tool_result pair that a peer's posted row split by `seq`. (3) the **prompt_requests org-parity trigger**
> (note 2) — defense-in-depth only (RLS already gates org membership; the child tables already have parity
> triggers). Both (2) and (3) are documented deferrals, not blockers.

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

**One schema change still outstanding (§6b Step 0): migration `64`** to re-key `slack_threads` from its
bare-UUID `session_id`/`root_request_id` (no FK; structurally survived 63 but reference dead rows) onto
`thread_id UUID REFERENCES threads(id)`. It's the only migration the remaining work needs.

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

**P6 core ✅** — the worker drives thread-feed turns.
- `runtime/worker.rs` rewritten around `ClaimedTurn`: `run()` claims via `claim_next_turn`; `handle_turn`
  resolves the agent, spawns heartbeat (`heartbeat_turn`) + cancel-watcher, runs the force-exit guard, then
  `release_turn`. The guard re-runs `reply_in_thread` (read-at-run) each attempt; no egress (`send_message`
  posted no row) ⇒ append an owner-private `system_note` nudge + retry; after `MAX_PINGPONG_RETRIES` ⇒
  `mark_turn_failed(NoEgress)`. RLS principal = `Caller::new(claim.acting_user_id, claim.org_id)`.
- New `PromptQueue` turn-finalise surface: `mark_turn_done` / `mark_turn_failed` / `heartbeat_turn` /
  `release_turn` (+ `TurnReceipt` from `ClaimedTurn::receipt()`, fenced on `lease_seq`). `ClaimedTurn` now
  carries `root_request_id` (drained from the trigger) → threaded into `reply_in_thread` so `send_message`'s
  budget bump + quiescence use the real DAG root (the P5-deferred fix).
- `status`/`statuses` SQL re-keyed off the dropped `session_id` to `COALESCE(state_id, background_turn_id)`.
- `WorkerPool::new` re-keyed: dropped `leases`/`sessions`/`pool`/`memory_store`/`clock`, added `threads`;
  the agent factory (`app.rs` + test harness + `runtime_pipeline.rs`) now wires `with_thread_store`.
- Test `tests/worker_thread_turn.rs::no_egress_nudges_then_fails` green; lib+tests clippy `-D warnings` + fmt clean.
**Remaining in P6 / deferred:** background cognition (`RequestKind::Reflection|Resolution`) is rejected as
unsupported until P8 builds the `background_turns` claim path (the old `run_background_kind` /
reflection-checkpoint / no-action-close logic was removed — rebuild in P8 from git history + this doc).
SSE delivery is best-effort on trigger ids (full stream re-key to `thread_id` is P10). The
`turn_metrics`/`tool_calls` recorders still `INSERT session_id` (broken since migration 63, best-effort §6) —
retype to `state_id` in **P11** when the old `run_turn` caller (which passes a real `SessionId`) is deleted.

**P7 core ✅** — membership-scoped thread reads (the query layer).
- `ThreadStore::list_threads(caller, channel_id)` + `ThreadSummary` (`thread_id`/`channel_id`/
  `last_activity_at`). Channel view: gated on `channel_members` + channel not archived, so **every member**
  sees the channel's threads regardless of who created them (not the single-creator/active-user pin). DM view
  (`channel_id = None`): the caller's own channel-less threads. **Org-pinned** (`caller.org_id` in the WHERE)
  so a multi-org member's other workspaces never leak (RLS gates membership, not the active org). `LIMIT
  MAX_THREAD_LIST` (§5). Runs `run_as_user(caller.user_id)`.
- Test `tests/threads_membership.rs::channel_feed_scoped_to_membership_not_active_user` (member B sees
  creator A's thread; non-member C sees none). lib+tests clippy `-D warnings` + fmt clean.
**Remaining in P7 / deferred:** the threads HTTP route (`http/routes/threads.rs`) still queries the dropped
`sessions`/`session_messages` — its rewrite onto `list_threads` + the flat-feed wire format is **P10**
(G1/G2). `RequestStatusView.session` keeps the bridged claim_key until the P10 status/stream re-key.

**P8 core ✅** — reflection rehomed onto background cognition (off the chat feed).
- New `background/` module: `BackgroundStore` + `PgBackgroundStore` (`create_turn`/`append`/`context`),
  `BackgroundTurnId` newtype, `NewBackgroundMessage`, `BackgroundError`. Messages in `background_turn_messages`
  (per-turn `MAX(seq)+1`). `pub mod background` in `lib.rs`.
- `RequestKindPayload::Reflection` re-keyed `{session_id, up_to_turn_id}` → `{thread_id, up_to_message_id}`
  (frozen slice). `NewTrigger.background_turn_id` typed `Option<BackgroundTurnId>`.
- Reflection scheduler rewritten to the thread model: `find_candidates` scans `agent_thread_state` +
  `thread_messages` (posted) + `reflection_checkpoints (agent_id, thread_id)`; `fetch_slice` reads the posted
  thread feed; `enqueue_reflection` creates a background turn, seeds the reflection prompt into it, and
  `enqueue_trigger`s a background trigger (no chat session).
- Agent `reply_background` + worker background branch (`run_background`): the P6 worker now *runs*
  `Reflection`/`Resolution` instead of rejecting them — reads the seeded prompt from `background.context`,
  replies into the background store, no ping-pong, no thread rows → `mark_turn_done`.
- Test `tests/reflection_pipeline.rs::reflection_writes_no_thread_message_rows` (worker-driven; reflection adds
  zero `thread_messages`, records the exchange in the background turn) + `tests/background_store.rs`. lib+tests
  clippy `-D warnings` + fmt clean.
**Remaining in P8 / deferred:**
- **Reflection checkpoint write** post-turn (advance `reflection_checkpoints (agent_id, thread_id)` to
  `up_to_message_id` on success) — without it the scheduler re-enqueues each idle window once the prior
  reflection is `Done`. Needs a checkpoint writer reachable from the worker (the P6 worker dropped its `pool`).
- **Resolution rehome**: the worker background branch handles `Resolution` kind, but the **librarian** still
  enqueues resolutions via the old session `enqueue` path (runtime-red) — rehome it to seed a background turn +
  `enqueue_trigger`, and port the no-action contradiction close as the resolution post-turn.
- **`ClaimKey` enum** (note 7): the worker discriminates chat vs background by `claim.kind` +
  `BackgroundTurnId::from(claim_key)` rather than a typed sum — fold into a `ClaimKey` enum if desired.
- `turn_metrics`/`tool_calls` recorder retype stays **P11** (still `INSERT session_id`).

A `/simplify` pass ran on P4–P8 code (per phase). Notable applied fixes: `tokio::join!` for the two
independent reads in `build_thread_request` (P4); extracted `deliver_to_human`/`deliver_to_agent` from
`send_message::handle` for §4 (P5); `PromptQueue` turn-finalise methods collapsed to call `finalise_turn`
directly (P6); `threads/limits.rs` + `ThreadSummary`→`ThreadListItem` rename (P7); worker background
reply-discard cleanup + `find_candidates` batch-limit assertion (P8). Skipped (with reasons): unifying the
thread/background turn loops (§4 — only 2 impls, they diverge on ping-pong/store/mapping); a context
row-LIMIT (append volume is already bounded by the `max_turns` loop).

**P9 core ✅** — scheduling is the third trigger source (thread model).
- `scheduled_tasks.channel_id` plumbed through `NewScheduledTask` / `ScheduledTaskRecord` /
  `PgScheduledTaskStore` (SELECT/INSERT/row map). `ScheduledTaskError` gains `Thread(#[from] ThreadError)`.
- `ScheduledTaskScheduler.fire` rewritten: resolve the task's human colleague → `create_thread` in
  `task.channel_id` (DM when `None`, created-by = the human) → `resolve_participation` → seed the task
  prompt as an **owner-private `system_note`** (the agent reads it read-at-run) → `enqueue_trigger`
  (`Normal`, `root_request_id = None` ⇒ fresh DAG, sender = human, receiver = agent,
  `trigger_message_id` = the seed) → `record_fired`. `spawn`/`spawn_with_cadence` gain `threads`.
  Old `queue.enqueue`/`NewPromptRequest::normal` path dropped from the scheduler (one less runtime-red caller).
- `ThreadStore::channel_of(thread)` added (privileged point lookup) so the tool can inherit the current
  thread's channel.
- Scheduling tools rehomed off the dropped `sessions`: `schedule_task` now sources org/user from
  `ctx.org_id`/`ctx.acting_user_id` and `channel_id` from `channel_of(ctx.thread_id)` (drops the
  `sessions` + `agents` deps, adds `threads`); `list`/`cancel` use `ctx.acting_user_id` for the
  `begin_as_user` gate instead of `sessions.tenancy(ctx.session_id)` (drop `sessions` dep). `app.rs`
  wiring updated for all three + the scheduler spawn.
- Test `tests/scheduling_thread_fire.rs::fire_creates_thread_and_agent_posts_summary_tagging_owner`
  (worker-driven: fire → thread in `#general` → agent `send_message`s a summary tagging the owner, gated
  by membership). `scheduling_pipeline.rs` adapted to the new model (its two scheduler-driven tests, red
  since P0, are green again; the old-path idempotency test replaced with an `enqueue_trigger` `sched-` key
  dedup test). `auth_scheduled_tasks` / `pg_scheduled_task_store` / `scheduling_routes` updated for the new
  `channel_id` field + spawn signature. lib + tests `clippy -D warnings` + fmt clean.
**Remaining in P9 / deferred:**
- **Concurrent double-fire** of the same instant on multiple scheduler nodes creates a thread *before* the
  idempotent `enqueue_trigger` dedups the trigger — leaving an orphan thread on the loser. Single-node
  (current) firing advances `next_run_at` via `record_fired` before the next tick, so it doesn't occur; the
  trigger itself is still deduped by the `sched-{task}-{fire}` key. Tighten (check idempotency before
  thread create, or unique-key the fire-thread) if multi-node scheduling lands.
- The scheduling **HTTP route** is read/cancel-only and lists explicit columns, so it neither needs nor
  surfaces `channel_id`; a future "show target channel" is a FE concern (P11).

**P10 G1/G2 ✅ (committed `b449e19`)** — the canonical flat feed + its read routes.
- `ThreadStore::feed(caller, thread, before_seq, limit)` + `FeedMessage` (`src/threads/{traits,pg_store,
  limits,mod}.rs`): the flat multi-party feed read — posted chat ∪ everyone's private artifacts, `kind`
  exposed, membership-gated + active-org-pinned, `seq` keyset paging (`MAX_THREAD_FEED = 500` /
  `DEFAULT_THREAD_FEED = 100`). After `/simplify`, **both participant sides are decoded once in the store**
  via the canonical `Participant::try_from`: `FeedMessage.sender: MessageSender` (System variant on a
  system row), `FeedMessage.receiver: Option<Participant>` (note 19). Opening test
  `tests/threads_feed.rs::g2_canonical_feed_seq_order_multi_party`.
- `http/routes/threads.rs` G1 (`list_threads` → `ThreadStore::list_threads`; wire `{thread_id, channel_id,
  last_activity_at}`) + G2 (`thread_messages` → `ThreadStore::feed` keyed by `{thread_id}`; wire exposes
  `kind` + `owner_agent_id` + `sender: MessageSender` + `receiver` + human name/avatar). The store is built
  **inline** from `state.pool`/`state.clock` (no `AppState.threads` field — note 18). G3 (`stream_thread`)
  is **unchanged** — still keyed on `root_request_id`, still runtime-red, rewritten in the rehome below.
- `/simplify` applied: collapsed the `FeedParticipant` double-decode (store decodes once; dropped the
  `FeedParticipant` type + two helpers); `tokio::try_join!` the two independent inserts in the scheduler's
  `initiate_thread`; refreshed the `AppState.pool` doc. Skipped: `AppState.threads` field (out-of-diff
  ripple, note 18), scheduler orphan-thread-on-double-fire (correctness, P9-deferred), feed visibility-gate
  dedup (premature-abstraction watch item, note 17a).

**P10 remaining — see §6b (the next session starts there).**

## 6b. P10 remaining — stream + slack + prompts rehome (START HERE next session)

This is the `root_request_id`/`session_id` → `thread_id` re-key. It is **one atomic landing**: the shared
types (`ResponseChunk::AgentMessage`, `ThreadStreamItem`) are read by slack, so you cannot compile a
partial. Opening test target: **`stream-by-thread`** (publish a chunk on a request carrying `thread_id`
T; assert a subscriber on T receives it) — add as `tests/thread_stream_by_thread.rs` (or fold into a
rewritten `threads_routes.rs`). Suggested order (each step keeps notes for *why*):

**Step 0 — migration `64_slack_threads_thread_id.{up,down}.sql` (CLAUDE.md §14: forward + tested down).**
`slack_threads.session_id` + `root_request_id` are **bare `UUID NOT NULL` columns with NO foreign key**
(migrations 29 + 35), so they structurally survived the `sessions` drop in 63 — but they now reference
dead rows. Re-key: drop `session_id` + `root_request_id`, add `thread_id UUID NOT NULL REFERENCES
threads(id) ON DELETE CASCADE` + `UNIQUE (thread_id)` (one Slack thread ↔ one Patom thread), keep the
`(team_id, channel_id, thread_ts)` PK + the org/workspace FK + the RLS policy. Reset waiver ⇒ no backfill
(`DELETE FROM slack_threads;` first if any rows survive). Bump dev DB to 64.

**Step 1 — stream types (the atomic core; all in one commit so the tree re-compiles):**
- `src/runtime/response.rs`: `ResponseChunk::AgentMessage { from, to_session: SessionId, content }` →
  `{ from, to_thread: ThreadId, content }`. Update the doc + `weight()` (still +36 for the uuid). Drop the
  `use crate::session::SessionId;` (AgentMessage was its only use here), add `crate::threads::ThreadId`.
- `src/runtime/pg_response.rs` `PUBLISH_CTE_SQL` (**note 16 — publish is RUNTIME-BROKEN today**): the `req`
  CTE `SELECT id, org_id, root_request_id, session_id FROM prompt_requests` still names the **dropped**
  `session_id` column, so every `publish()` errors. Change `req` to select `thread_id` (prompt_requests
  has it since migration 63), and the `pg_notify` JSON to emit `'thread_id', req.thread_id` instead of
  `root_request_id`/`session_id`. NOTE `thread_id` is NULL on a background-turn trigger — emit it anyway
  (JSON null) and have the listener skip null (background turns have no SSE subscriber).
- `src/runtime/pg_thread_stream.rs`: `NotifyPayload` → `{ request_id, thread_id: Option<ThreadId>,
  chunk_seq }`. Slot table `HashMap<PromptRequestId, ThreadSlot>` → `HashMap<ThreadId, ThreadSlot>`;
  `subscribe(root: PromptRequestId)` → `subscribe(thread: ThreadId)`; `handle_notification` routes by
  `payload.thread_id` (skip `None`); `ThreadStreamItem.session_id: SessionId` → `thread_id: ThreadId`;
  `fetch_item` builds it from the payload (the `AgentMessage { from, .. } → from` vs `receiver_agent_id`
  branch stays). Drop the `SessionId` import.

**Step 2 — G3 route (`src/http/routes/threads.rs::stream_thread`):** path id is now a `thread_id`. Replace
the `SELECT id FROM sessions WHERE root_request_id…` visibility gate with a thread-visibility check (reuse
the `FEED_SQL` gate shape, or add a small `ThreadStore::visible_to(caller, thread) -> bool`; the inline
`PgThreadStore` pattern from G1/G2 applies). `subscribe(thread_id)`. **Decision (recommend):** the thread
feed is *continuous* — do **NOT** close the SSE on a per-DAG terminal chunk (remove the close-on-terminal
`scan`); a `Done`/`Error` chunk is a per-turn marker the FE uses, not a stream close. Note-10's
"terminal = no live lease on any `(thread,*)`" thread-level quiescence is the deeper model — defer it (the
per-DAG `Done` from `worker::maybe_emit_quiescence` still fires and is harmless as a marker). Update the
module-doc header (it still says G3 is `root`-keyed).

**Step 3 — `POST /prompts` @tag rehome (`src/http/routes/prompts.rs`; blocks slack + note 12).** Today
`submit_internal`/`SubmitPromptParams` use `NewPromptRequest` + `queue.enqueue_for_user` (session mint) +
`session_agent_participant` (`sessions.participants`, runtime-red). Rehome: **new root** (no `thread_id`)
→ `create_thread` in `channel_id` (DM when none) + `append` the human's `kind='posted'` row (tagging the
agent) + `resolve_participation` + `enqueue_trigger` (root mint, `trigger_message_id` = the posted row);
**continuation** (`thread_id` given) → `append` posted row + `enqueue_trigger`. Wire shape: replace
`session_id` continuation with `thread_id`; keep `agent_id` (@tag target) + `channel_id` (new root).
**Ripples:** `auth/callback.rs` builds `SubmitPromptParams` for the MCP-resume prompt (carries a
`session_id` from `mcp_oauth_pending`) — rehome the resume to append-to-thread + trigger; the
`mcp_oauth_pending` row's `session_id` becomes `thread_id` (check that table's schema). Then rewrite
`tests/prompts_routes.rs`.

**Step 4 — slack rehome (the big one; `src/slack/{thread_map,bridge,stream_pump}.rs`).**
- `thread_map.rs`: `ThreadMapping { org_id, session_id, root_request_id }` → `{ org_id, thread_id }`;
  `lookup_by_session(session_id)` → `lookup_by_thread_id(thread_id)`; `bind_root(…, session_id, root)` →
  `bind(…, thread_id)`; SQL onto the migration-64 columns; update `FakeSlackThreadStore` (used by tests).
- `bridge.rs`: the inbound mention path submits via the old session prompt path + `session_agent_
  participant(mapping.session_id)` (bridge.rs:204) — rehome onto the **Step-3** thread path: a Slack
  `(team,channel,thread_ts)` maps to a Patom thread; first mention `lookup_by_thread` miss ⇒ create_thread
  + bind; then append posted + enqueue_trigger. Reuse the Step-3 logic (factor a shared `submit_to_thread`
  helper if clean).
- `stream_pump.rs` (1282 lines): `deps.thread_stream.subscribe(req.root)` → `subscribe(thread_id)`; route
  by `item.thread_id` (was `item.session_id` / `to_session`). The per-`(agent,human)`-session **mint**
  logic (`routing_for`, `allow_mint`, `lookup_by_session`) **simplifies**: one Patom thread = one Slack
  thread, so a descendant agent reaching the human posts into the *same* Slack thread (the mint-a-fresh-
  top-level-message branch likely collapses — verify against the pump tests at lines ~1089-1200). Update
  `AgentMessage { to_thread, .. }` reads. This is the highest-risk file; lean on its existing unit tests.

**Step 5 — `get_session` tool (`src/tools/system/get_session.rs`).** Built entirely on `SessionStore`
(`snapshot_window`/`participants`/`root_request_id`). In the thread model the agent already reads its full
thread at run-time (read-at-run), so cross-session lookup is largely **obsolete**. **Decision (open):**
either delete it in P11 (preferred — the read-at-run context subsumes it) or rehome it to read the thread
tail via `ThreadStore::feed`. Until decided it stays runtime-red (compiles).

**Step 6 — `RequestStatusView.session` + re-add SSE notify.** `RequestStatusView.session` holds the
bridged `COALESCE(state_id, background_turn_id)` claim_key (a `SessionId`-typed bridge) — retype/rename to
`thread_id` (or `claim_key`) when the status view is reworked here. Re-add the P5-dropped human-delivery
SSE notify on the now thread-keyed stream (send_message → publish an `AgentMessage`/notify on the thread).

**Tests to rewrite (runtime-red on the old shape):** `tests/threads_routes.rs` (G1/G2/G3 assert
`root_request_id` rows), `tests/slack_e2e.rs`, `tests/prompts_routes.rs`, plus audit `tests/{quiescence,
runtime_pipeline,turns_routes,session_resolve}.rs` for session-keyed assertions. `tests/pg_session_store.rs`
is deleted with the `session/` module in P11.

## 6a. Carried-forward deferred TODOs

Cross-phase loose ends. None block the committed green tests; they are the difference between "opening
test passes" and "feature is production-correct". **The P10 HTTP/slack/stream items now live in §6b** (the
ordered rehome plan) — this section keeps only the cognition follow-ups + the P11 sweep list.

**Cognition follow-ups (P8, still open — independent of the §6b rehome):**
- **Reflection checkpoint write.** After a successful background reflection the worker must advance
  `reflection_checkpoints (agent_id, thread_id).last_message_id = up_to_message_id` (from the
  `Reflection` payload). Without it the scheduler re-enqueues the same reflection every idle window once
  the prior one is `Done`. The P6 worker dropped its `pool`, so this needs a checkpoint writer reachable
  from the worker (a small store method, or re-thread `pool`/a writer into the worker's background branch).
  **Cost:** a `WorkerPool::new` signature ripple to the ~5 pool call sites (`app.rs`, `tests/common/
  harness.rs`, `worker_thread_turn`, `runtime_pipeline`, `scheduling_thread_fire`).
- **Resolution rehome.** The worker background branch *runs* `Resolution` kind, but the **librarian**
  (`memory/librarian.rs`) still enqueues resolutions via the old session `enqueue` path (runtime-red).
  Rehome it like the reflection scheduler: seed a `background_turns` turn with the resolution prompt +
  `enqueue_trigger` a background trigger; port the no-action contradiction close as the resolution
  post-turn (old logic in git history pre-`f21bfd4`: `close_no_action_if_unresolved`).

**P11 (sweep + gates):**
- Retype `turn_metrics`/`tool_calls` recorders `session_id` → `state_id` (migration 63 renamed the columns;
  the recorders still `INSERT session_id` → runtime-broken, best-effort §6). Do it **with** the old
  `run_turn` deletion (the old caller passes a real `SessionId`).
- Delete the old `session/` module + old queue methods (`enqueue`/`claim_next_session`/pair `mark_*`) +
  `Participant::canonical_pair`/`canonical_cmp` + the inherent/trait `enqueue_trigger`/`claim_next_turn`
  duplication (collapse the `Self::`-delegation in `pg_queue.rs` to single defs).
- Wire `todos` into the thread path; re-pair `tool_use`/`tool_result` at context-build (note 13).
- Web FE (`web/src/...` + `web/mock-backend.ts`); all gates (`cargo fmt --all -- --check`,
  `clippy --all-targets -- -D warnings`, `check --all-targets`, `nextest`, `deny`/`audit`).

## 7. Discovered gaps / handoff notes (read before resuming)

> **Status as of P10-partial:** notes 1, 3, 5, 8, 9, 11 are durable facts (still true). **Note 4**
> (`seed_prompt_request` broken) — still broken, new-path tests avoid it. **Note 6** (recorder retype) →
> **P11**. **Note 7** (`ClaimKey`) — `BackgroundTurnId` is a real newtype; the typed `ClaimKey` sum still
> deferred. **Note 12** (@tag routing) → **§6b Step 3** (the rehome). **Note 13** (tool_use/result re-pair)
> → **P11**. **Note 14** (idempotency) — applied. **Note 15** (scheduled-target membership) — satisfied by
> P9 (the scheduled agent's `send_message` to the owner hits the channel-member gate; the opening test
> covers it via `#general`). **Notes 2, 10** open (2 = parity trigger, P11; 10 = stream re-key, **§6b**).
> **New notes 16–20** below are P10-partial discoveries — read them before the rehome.

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
    (send_message human-gate), else the post rejects. **Satisfied in P9** (opening test fires into
    `#general`, where the task owner is auto-enrolled, so the agent's `send_message` to them passes).

16. **`pg_response` publish is RUNTIME-BROKEN today (since migration 63).** `PUBLISH_CTE_SQL`'s `req` CTE
    `SELECT id, org_id, root_request_id, session_id FROM prompt_requests` names the **dropped** `session_id`
    column, so every chunk `publish()` / `publish_for_user()` errors at runtime. It compiles (raw SQL
    string). The §6b stream re-key fixes this *and* re-keys the NOTIFY in one change — don't treat the
    stream re-key as optional polish; live streaming is currently down. (Worker turns still complete +
    persist feed rows; only the SSE/NOTIFY tap is broken — the FE refetches G2.)
17. **Slack is session-coupled end-to-end and the re-key is atomic.** `slack/thread_map.rs` keys
    `slack_threads` by `session_id`+`root_request_id` (both bare `UUID NOT NULL`, **no FK** — migrations 29
    + 35 — so they survived the `sessions` drop structurally but reference dead rows). `slack/bridge.rs`
    submits via the old session prompt path; `slack/stream_pump.rs` (1282 lines) routes every chunk by
    `item.session_id`/`to_session` and mints per-`(agent,human)`-session Slack messages. Re-keying
    `ResponseChunk::AgentMessage.to_session` / `ThreadStreamItem.session_id` to `thread_id` breaks slack's
    compile immediately ⇒ the stream re-key + slack rehome + a `slack_threads` migration (§6b Step 0) +
    the `POST /prompts` rehome the bridge depends on **all land together**. Budget accordingly.
    - **17a.** The G2 feed visibility `CASE/EXISTS` gate (`FEED_SQL`) now spells out the same
      member-or-DM-owner + not-archived predicate as `list_threads` and (partially) `is_channel_member` —
      rule-of-three is *just* met. `/simplify` left it inline (the three differ enough that a forced shared
      fragment risks premature abstraction). If a 4th thread-visibility query appears, extract a
      `thread_visible_to(user)` SQL fragment / `ThreadStore::visible_to` then. Also: `is_channel_member`
      omits the archived-channel check the other two enforce — a latent drift to reconcile.
18. **`AppState` has no `threads` field — G1/G2 build `PgThreadStore` inline** from `state.pool` +
    `state.clock` (two `Arc` clones/request). `/simplify`'s altitude pass flagged that a proper
    `AppState.threads: SharedThreadStore` (cloned from the one already built at `app.rs` and injected into
    worker/scheduler/tools) is the cleaner altitude, but adding the field ripples to ~12 `AppState { … }`
    literals across the HTTP test harnesses (outside the diff), so it was skipped. The §6b stream route
    (`stream_thread`) + a possible `ThreadStore::visible_to` are more `state.threads` callers — if you add
    them, that's the moment to weigh adding the field vs. continuing inline. The `AppState.pool` doc was
    updated to stop claiming thread SQL is inline-in-route.
19. **`FeedMessage` is fully decoded (post-`/simplify`).** `FeedMessage.sender: MessageSender` (System
    variant on a system row), `FeedMessage.receiver: Option<Participant>` — decoded **once in the store**
    (`feed_row_to_message` → `decode_participant` → the canonical `Participant::try_from(ColleagueColumns)`
    at `types/participant.rs`). The G2 wire `ThreadMessage` (`http/routes/threads.rs`) mirrors that +
    `sender_display_name`/`sender_avatar_url` enriched from the privileged user store. There is no
    `FeedParticipant` type any more — don't reintroduce a raw-columns intermediate.
20. **Scheduler `initiate_thread` overlaps two inserts** (`/simplify`): after `create_thread`, the
    `resolve_participation` + seed `append` run under one `tokio::try_join!` (disjoint tables, no ordering
    dep). The orphan-thread-on-concurrent-double-fire risk (P9 "Remaining") is unchanged by this — still a
    single-vs-multi-node concern, still deferred.

## 8. Remaining phases — Resume here

**Committed green: P0–P9 + P10 G1/G2** (`b449e19`, §6). Remaining: **the P10 stream+slack+prompts rehome,
then P11.** Each opens with a failing test (CLAUDE.md §3).

1. **P10 rehome → start at §6b** (the ordered, file-by-file plan: migration 64 + the atomic stream-type
   re-key + G3 route + `POST /prompts` @tag + slack + `get_session` + status-view/SSE-notify + test
   rewrites). Opening test `stream-by-thread`. Read notes **16–20** (§7) + §6a first.
2. **P11 sweep + FE → see §6a "P11"**: recorder `session_id→state_id` retype (with old `run_turn`
   deletion); delete the old `session/` module + old queue methods (`enqueue`/`claim_next_session`/pair
   `mark_*`) + pair tests + `Participant::canonical_pair`/`canonical_cmp` + the `Self::`-delegation
   collapse in `pg_queue.rs`; re-add the thread/state-scoped parity trigger (note 2); wire `todos` into the
   thread path; `tool_use`/`tool_result` re-pair (note 13); decide `get_session` (delete vs rehome, §6b
   Step 5); web FE (`web/src/*` + `web/mock-backend.ts`) onto the new G1/G2/G3 wire shapes; all gates
   (`fmt`, `clippy --all-targets -D warnings`, `check --all-targets`, `nextest`, `deny`/`audit`).

The P8 cognition follow-ups (§6a: reflection-checkpoint write, resolution rehome) are independent of the
rehome — close them whenever; the reflection-checkpoint write needs a checkpoint writer reachable from the
worker (which dropped its `pool` in P6) so it carries a `WorkerPool::new` ripple to the ~5 pool call sites.

## 9. Verification / how to run

- Local PG: `docker compose up postgres` (DATABASE_URL in `.env`).
- Per-phase test: `set -a && . ./.env && set +a; cargo test -p patom-core --test <name>`.
- Migration round-trip: `sqlx migrate run --source crates/patom-core/migrations` then `… revert`.
- Final gates (P11): `cargo fmt --all -- --check`, `cargo clippy --all-targets --all-features
  -- -D warnings`, `cargo check --all-targets --all-features`, `cargo nextest run --all-features`.
- FE: extend `web/mock-backend.ts`, then preview tools (`preview_start` / `preview_screenshot`).

## 10. Key files

- Schema: `crates/patom-core/migrations/00000000000063_thread_feed.{up,down}.sql`
- Thread store (P1): `src/threads/{mod,error,traits,pg_store,limits}.rs`
- Background store (P8): `src/background/{mod,error,traits,pg_store}.rs`
- Queue (P2/P6): `src/runtime/{queue,pg_queue,dag}.rs` (`NewTrigger`/`ClaimedTurn`/`TurnReceipt`,
  `enqueue_trigger`/`claim_next_turn`/`mark_turn_*`/`heartbeat_turn`/`release_turn`)
- Agent loop (P4/P8): `src/agent_core/{builder,core,turn,error,outcome}.rs` (`reply_in_thread`,
  `reply_background`); memory (P4): `src/memory/{traits,static,agent,loader}.rs`
- Worker (P6/P8): `src/runtime/worker.rs` (`claim_next_turn` loop, ping-pong guard, `run_background`)
- send_message (P5): `src/tools/system/send_message.rs`
- Reflection scheduler (P8): `src/memory/reflection_scheduler.rs`
- Thread feed read (P10 G1/G2, committed): `src/threads/{traits,pg_store}.rs` (`feed` + `FeedMessage` +
  `channel_of`); `src/http/routes/threads.rs` (G1 `list_threads` + G2 `thread_messages`; G3 `stream_thread`
  still root-keyed/runtime-red).
- Scheduling (P9, committed): `src/scheduling/{scheduler,pg_store,store,types,error}.rs`,
  `src/tools/system/scheduling/*`.
- Composition root: `src/app.rs` (`Collaborators` + `AgentFactoryPieces` wire threads/background stores)
- **Rehome targets (§6b):** migration `64` (`slack_threads`); stream — `src/runtime/{response,pg_response,
  pg_thread_stream}.rs` + `src/http/routes/threads.rs::stream_thread`; prompts — `src/http/routes/prompts.rs`
  + `src/auth/callback.rs`; slack — `src/slack/{thread_map,bridge,stream_pump}.rs`; `get_session` —
  `src/tools/system/get_session.rs`. **P11:** `src/session/*` (delete), the `turn_metrics`/`tool_calls`
  recorders + old `run_turn`, `src/runtime/pg_queue.rs` (`Self::`-delegation collapse), `web/*`.
- New-path tests landed: `tests/{pg_thread_store,pg_turn_queue,turn_dag_mint,agent_thread_loop,
  agent_thread_send_message,worker_thread_turn,threads_membership,background_store,reflection_pipeline,
  threads_feed,scheduling_thread_fire}.rs`
- Tests to rewrite in the rehome (runtime-red): `tests/{threads_routes,slack_e2e,prompts_routes}.rs` (+
  audit `quiescence,runtime_pipeline,turns_routes,session_resolve`). `tests/pg_session_store.rs` is deleted
  with `session/` in P11.
- Still broken/unused: `tests/common/pg.rs::seed_prompt_request` (note 4 — new tests avoid it).
