# Plan — Chat UI Slack-parity (post thread-chat refactor)

> **Status: SHIPPED (2026-06-10).** All steps landed in one commit on
> `feat/thread-chat-refactor`; dev DB at migration `66`. Gates green:
> `cargo fmt --check`, `clippy --all-targets --all-features -- -D warnings`
> (0 findings), `cargo check --all-targets --all-features`, full
> `cargo test -p patom-core` (49/49 binaries), FE `tsc` + `bun run build`,
> migration 66 up/down/up round-trip, mock-backend contract smoke-tested via
> curl (untagged post, multi-tag, DM implicit trigger, pair filter, member
> profiles). Browser preview tooling was unavailable this session — visual
> verification of the new timeline/sidebar is the one outstanding manual step.

**Delivery: ONE commit on the current PR** (`feat/thread-chat-refactor`). No phase splitting —
the build order below is the internal work order (TDD red-first per behavior, CLAUDE.md §3), but
everything lands together: BE wire + migration + FE + tests + mock-backend, gates green at the end.

## Context

The backend thread-feed refactor (doc/thread-chat-refactor.md) is complete, but the chat UI still
encodes the **old pair-model restrictions**: messages must tag an agent, every reply is silently
routed to a "default agent", DMs are agent-only, and humans are second-class in the mention/roster
machinery. Target behavior (locked with the user, 2026-06-10):

- Everyone in a channel participates — humans and agents are **the same kind of chat participant**
  in the UI. You can @tag an agent or a human.
- **An agent runs only when tagged** in channels. An untagged channel message is a plain post.
- **The default-agent concept is deleted end-to-end.** The recruiter remains as a *preset* agent
  created at onboarding, nothing more.
- The app behaves like Slack: channel timeline of root messages + reply threads.
- **DMs include human members**, not just agents.
- **Agent→agent invocation works everywhere, including inside a human↔agent DM thread** — the DM
  counterpart pair is the *conversation container*, not a participant cap. (Verified: backend
  `send_message` → agent receiver has no membership gate — agents are org-global — and the joining
  agent gets its own `agent_thread_state` in the same thread. Nothing in this plan may regress
  that.)

## What the scan found

### Frontend — implicit old-model logic to remove

| # | File | Behavior |
|---|---|---|
| F1 | `web/src/components/organisms/Composer.tsx` | `channelBlocked` refuses to send a channel message without an agent tag; "tag an agent (e.g. @x) to send" copy; only the **first** @tag routes; DM mode auto-routes to `dmAgent`; `tipExample` reads `is_default`. |
| F2 | `web/src/components/molecules/MentionInput.tsx` | Mention candidates are **agents only**; "active mention" highlight only for the first tag in channel mode; renders a `default` badge. |
| F3 | `web/src/pages/ChatView.tsx` | `defaultAgent = agents.find(is_default) ?? agents[0]`; `onSubmit` aborts with **no agent** (`if (!agent_id) return`); `onThreadReply` silently prefixes `@replyAgent` (selected DM agent or the default) onto every thread reply. |
| F4 | `web/src/components/organisms/Sidebar.tsx` | "Direct messages" section lists **agents only**; "New DM" button is dead. |
| F5 | `web/src/components/organisms/ChannelHeader.tsx` | Shows org-wide "N agents", not channel membership. |
| F6 | `web/src/components/organisms/MessageList.tsx` | Channel timeline rows are bare "Thread · View thread" stubs — no root message, author, or reply count (not Slack-like). |
| F7 | `web/src/lib/foldHistory.ts` | `rootMessage` = first **human** row (breaks scheduled/agent-initiated threads); `receiverFrom` drops human receivers; `prefixWithReceiver` resolves agent names only. |
| F8 | `web/src/lib/mentions.tsx` + render sites (`ThreadPanel`, `ChatView`) | `renderMentions(text, agents.map(a => a.name))` — human mentions never highlight. |
| F9 | `web/src/hooks/useThreadView.ts` | `showThinking` whenever the last bubble is an unpersisted human message — wrong once untagged posts exist (nothing will ever reply). |
| F10 | `web/src/types/api.ts`, `lib/api.ts`, `lib/demo.ts`, `components/onboarding/StepChooseTeam.tsx` | `Agent.is_default` baked into types, create payloads, demo fixtures. |
| F11 | `web/src/pages/ChatView.tsx` deep-link | `/?turn=<id>` sets `selectedRoot = turn.root_request_id`, but threads are keyed by `thread_id` since P10 — the deep-link G2 fetch can never match. Pre-existing bug, fix while here. |

### Backend — wire gaps the new UI needs

| # | Where | Gap |
|---|---|---|
| W1 | `http/routes/prompts.rs` | `submit_internal` **always** resolves an agent (explicit → `last_agent` → `agents.default_id_for`) and always enqueues exactly one trigger. No untagged post; no multi-tag (`@X @Y` = two DAGs per the locked design §5); no human tags. |
| W2 | `agents/*`, `slack/bridge.rs:468` | `is_default` column + `default_id_for`; Slack mention path resolves the org default agent. |
| W3 | `threads/pg_store.rs` | DM = channel-less thread visible to its **creator only** (`created_by_colleague_id`). No counterpart, so a human↔human DM is unrepresentable and the recipient could never see it. |
| W4 | `http/routes/threads.rs` G1 | `{thread_id, channel_id, last_activity_at}` only — too thin to render a Slack timeline (no root snippet/author/reply count). |
| W5 | `http/routes/channels.rs` `list_members` | Returns `{user_id, added_at}` — no display name/avatar, so humans can't populate a mention roster. (Precedent for the fix: the privileged `users.read_profiles` enrichment from the multi-user display-identity work.) |
| W6 | `http/routes/turns.rs` | `TurnDetail` carries `root_request_id` but not `thread_id` → F11 can't be fixed FE-only. |
| W7 | `tools/system/send_message.rs` `deliver_to_human` | `is_channel_member` returns `true` for **any** human on a channel-less thread. With real DM pairs this must tighten to {creator, counterpart} — while the **agent-receiver path stays ungated** (org-global), preserving agent→agent invocation inside DMs. |

## Decisions (locked)

1. **DM to an agent invokes it without a tag.** "Agent runs only when tagged" governs channels;
   in a 1:1 DM the counterpart is the implicit addressee (Slack: DM-ing a bot pings it). A human
   counterpart just receives a plain post.
2. **Agent→agent works inside DMs.** An agent in a human↔agent DM may `send_message` any other
   agent; the invoked agent participates in the same DM thread (own `agent_thread_state`, own
   private artifacts, posts visible in the DM feed). The counterpart column constrains *human
   visibility*, never agent participation. Pin with a test (see T-list).
3. **DM modeling: counterpart column, not pair-channels.** Add
   `threads.dm_counterpart_colleague_id` (NOT NULL ⇔ `channel_id IS NULL`). A DM "conversation"
   with colleague C = all channel-less threads where {creator, counterpart} = {me, C}.
4. **Multi-tag wire**: FE parses mentions (it owns the roster) and submits explicit
   `tags: [{kind, id}]`; BE does **not** parse message text. One trigger + fresh DAG per tagged
   *agent*; tagged humans are stored for render/notify. `thread_messages.receiver_colleague_id`
   stays single-valued = first tag (mentions live in the text anyway).
5. **Slack bridge** loses `default_id_for` → route `@Patom` mentions to the org's **recruiter
   preset** (a preset lookup replaces "default"). Per-install agent choice is a later feature.
6. **No notification system yet** for human tags/DMs — tagging a human renders + (already) feeds
   their context; push/badge UX is out of scope here.
7. **Breaking wire changes are fine** — FE and BE land in the same commit; no `agent_id`
   compatibility shim on `/prompts`.

## Build order (one commit; steps are work order, not commits)

### Step 0 — Migration `66_chat_slack_parity.{up,down}.sql`
One paired migration, all schema deltas of this change:
- `ALTER TABLE agents DROP COLUMN is_default;`
- `ALTER TABLE threads ADD COLUMN dm_counterpart_colleague_id UUID NULL REFERENCES colleagues(id)`
  + CHECK (`(channel_id IS NULL) = (dm_counterpart_colleague_id IS NOT NULL)`).
- Backfill existing DM threads: counterpart := the thread's single `agent_thread_state` agent's
  colleague; zero/multi-agent strays → the org's recruiter (first-created agent). Log counts.
- (If chosen below) `thread_messages.idempotency_key TEXT NULL` + partial unique index for
  untagged-post dedupe.
- Tested down (drops column/check, re-adds `is_default DEFAULT false` — lossy default OK).
- RED test first: migration up/down/up round-trip on scratch DB; dev DB bumped to 66.

### Step 1 — BE: tags drive triggers; untagged posts are legal (`prompts.rs`, queue)
- `SubmitPromptRequest`: `agent_id` → `tags: Vec<TagRef>` (`{kind: "agent"|"human", id}`, cap
  `MAX_TAGS_PER_MESSAGE` in `http/limits.rs` or `threads/limits.rs`).
- Zero tags ⇒ append the `posted` row only, **no trigger**; response
  `{request_id: null, thread_id, triggered_agent_ids: []}`.
- N agent tags ⇒ N `enqueue_trigger` calls (idempotency `"{key}:{agent_id}"` so a retry can't
  double-trigger any single agent), each a fresh DAG. Response carries `triggered_agent_ids`.
- Delete the `last_agent` + `default_id_for` fallbacks **except**: continuation into a DM thread
  whose counterpart is an agent ⇒ implicit trigger for that agent (decision 1). Source the
  implicit receiver from `dm_counterpart_colleague_id`, not `last_agent` (a third agent invoked
  mid-DM must not steal subsequent untagged replies).
- New-DM root: `channel_id` absent ⇒ require `counterpart` (colleague id) in the payload; stamp
  the column at `create_thread`.
- Untagged-post idempotency: retries of a no-trigger post can't be deduped via
  `find_existing_trigger` — dedupe on `thread_messages.idempotency_key` (Step 0 column).
- MCP-OAuth resume (`auth/callback.rs` → `SubmitPromptParams`): carries `agent_id` today — maps
  to `tags: [agent]`; verify resume e2e still green.

### Step 2 — BE: delete the default agent (`agents/*` + Slack)
- Remove `default_id_for` from `AgentStore`; drop `is_default` from agent routes/types/
  `create_agent` tool ripples.
- `slack/bridge.rs:468` → recruiter-preset lookup (decision 5). Keep slack e2e green.

### Step 3 — BE: people on the wire (roster + DM visibility + G1/turns enrichment)
- `GET /channels/{id}/members` rows gain `display_name`, `avatar_url`, `email` via the privileged
  user read (mirror the G2 enrichment; active-org-pinned per the RLS-membership memory).
- DM visibility everywhere (`list_threads`, `FEED_SQL`, `visible_to`, `is_channel_member`):
  channel-less thread ⇒ visible to creator **or counterpart** (when human). `GET /threads` DM
  mode gains `?counterpart=<colleague_id>` (the pair filter, both directions).
- `send_message::deliver_to_human` DM gate (W7): human receiver must be the DM's creator or
  counterpart — reject others with the same no-auto-add error. **Agent receiver path untouched.**
- G1 rows gain `{root: {snippet, sender, sender_display_name, sender_avatar_url}, reply_count}`
  — one query, `LEFT(body, N)` snippet cap per §5.
- `TurnDetail` gains `thread_id` (W6).

### Step 4 — FE: composer + mentions treat humans and agents alike
- New `Mentionable = {kind: "human"|"agent", id, colleague_id, name, avatar_url}`; roster per
  context: channel → enriched channel members ∪ all agents (org-global); DM → counterpart +
  all agents (agents stay taggable in DMs — decision 2); thread → its channel's roster.
- `MentionInput`/`mentions.tsx`: generalize `Agent[]` → `Mentionable[]` (human monogram vs bot
  mark; delete the `default` badge). Every known-name tag highlights — kill first-tag-only logic.
- `Composer`: delete `channelBlocked`, the "tag an agent to send" copy, the routing TipBar;
  emit `tags` = all matched mentions. Placeholder: "Message #channel — @ to mention".
- `ChatView.onSubmit`: drop `defaultAgent` + the `if (!agent_id) return` guard.
  `onThreadReply`: **delete the auto-`prefixMention`** — replies send exactly what was typed.
- `useSubmitPrompt`/`api.submitPrompt`: `tags` + `counterpart`; `SubmitPromptResponse` retypes
  `request_id: string | null` + `triggered_agent_ids`.
- `useThreadView.showThinking`: keyed on the pending entry's `triggered_agent_ids` (no trigger ⇒
  no thinking placeholder). Stretch: per-agent "X is typing" off live store entries.
- Remove `is_default` everywhere (F10).

### Step 5 — FE: DMs with humans (Sidebar + ChatView modes)
- Sidebar "Direct messages": list **colleagues** — humans (org roster) + agents — each row
  opening that counterpart's DM feed (`useThreads(null, counterpartColleagueId)`); wire the dead
  "New DM" button to a colleague picker.
- `ChatView`: `selectedAgentId` → `selectedCounterpart: Mentionable | null`; DM composer submits
  `counterpart` on new roots; agent counterpart keeps the "replies route to them" hint, human
  counterpart gets a plain-chat placeholder. A third agent's posts/typing render in the DM feed
  exactly as in channels (decision 2 — no DM-special-casing in the feed renderer).
- `useThreadStream`/`threadStore` unchanged (already thread-keyed).

### Step 6 — FE: Slack-style channel timeline + polish
- `MessageList`: render each G1 row as a root message (author monogram + name + snippet + time +
  "N replies →"), grouped by day. Empty-state copy drops the DAG jargon.
- `foldHistory`: `rootMessage` = first **posted** row regardless of sender kind (F7); human
  receivers resolved in `prefixWithReceiver`/`receiverFrom` via the roster; full mentionable name
  list into `renderMentions` (F8).
- `ChannelHeader`: channel member count (+ agents) from the roster, not org-wide agent count.
- Fix `/?turn=` deep-link via `TurnDetail.thread_id` (F11).
- i18n (`en.ts` + `vi.ts`); `web/mock-backend.ts` + `lib/demo.ts` updated for every new wire
  shape (tags, counterpart, G1 root summary, member profiles) so the preview runs.

## Test list (each written RED before its step's implementation)

| Test | Pins |
|---|---|
| `migration 66 up/down/up` | Step 0 |
| `prompts_untagged_post_appends_without_trigger` | untagged = plain post, `request_id: null` |
| `prompts_two_tags_two_triggers_two_dags` | locked design §5 multi-tag |
| `prompts_dm_agent_counterpart_implicit_trigger` | decision 1 |
| `prompts_dm_untagged_reply_after_third_agent_still_routes_to_counterpart` | implicit ≠ last_agent |
| `send_message_agent_to_agent_inside_dm_triggers_and_posts` | **decision 2 (user-pinned)** |
| `send_message_dm_human_receiver_outside_pair_rejected` | W7 tighten |
| `dm_counterpart_sees_thread_creator_and_back` | W3 visibility both directions |
| `g1_rows_carry_root_summary_and_reply_count` | W4 |
| `channel_members_carry_profile` | W5 |
| `turn_detail_carries_thread_id` | W6 |
| `slack_mention_routes_to_recruiter_preset` | decision 5 |
| `agents_wire_has_no_is_default` | Step 2 |
| FE: `tsc` + `bun run build` + mock-backend preview (channel untagged post, multi-tag, human DM round-trip, agent-in-DM invoking second agent, timeline render) | Steps 4–6 |

## Exit gates (the single commit is not done until all green)

`cargo fmt --all -- --check` · `cargo clippy --all-targets --all-features -- -D warnings` ·
`cargo check --all-targets --all-features` · `cargo nextest run --all-features` (Docker PG) ·
migration 66 round-trip · FE `tsc` + build · preview screenshots.

## Risks / must-not-miss

- **R1 — DM backfill (Step 0).** Existing channel-less threads need a counterpart; derive from
  the thread's single `agent_thread_state` row. Zero/multi strays → recruiter; log a count.
- **R2 — Untagged-post idempotency (Step 1).** No trigger row ⇒ `find_existing_trigger` can't
  dedupe; the `thread_messages.idempotency_key` path must cover it.
- **R3 — Multi-tag idempotency keys (Step 1).** Per-agent suffix must respect the key length cap
  and the `tag:` conventions (refactor doc note 14).
- **R4 — `showThinking` (Step 4).** Composer changes and `triggered_agent_ids` plumbing are one
  unit, or every untagged post shows an eternal "thinking…". Single commit makes this safe — just
  don't lose it in review.
- **R5 — Implicit DM receiver source (Step 1).** Must be `dm_counterpart_colleague_id`, NOT
  `last_agent` — otherwise a third agent invoked mid-DM (decision 2) captures the human's
  subsequent untagged replies.
- **R6 — Slack bridge (Step 2).** `default_id_for` removal breaks compile; recruiter-preset
  lookup lands in the same step. Slack e2e stays green.
- **R7 — Roster privacy (Step 3).** Member enrichment uses the privileged user read pinned to the
  active org (memory: RLS gates membership, not active org).
- **R8 — One-commit discipline.** This intentionally violates the §13 one-logical-change default
  per the user's explicit instruction; the PR description must say so and enumerate the surfaces
  (wire break on `/prompts`, migration 66, FE rehome) so review isn't surprised.
