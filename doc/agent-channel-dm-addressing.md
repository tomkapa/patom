# Agents address channels & DMs like members

Status: **design** (2026-06-15). Evolves `doc/thread-chat-refactor.md` (send_message
as the sole verb) and the locked thread-chat design. Decisions baked in below were
confirmed by the product owner.

## Goal

An agent should reach any place it has access to, **like a human colleague**:

- reply in the thread it is currently in (today's only behavior), **or**
- start a new thread in a channel it is a **member** of, **or**
- DM any colleague it can see (raise a question to the right person).

This must work from **every** trigger source — an inbound chat message, an
agent→agent hand-off, **and the scheduler** — not just inbound. The original bug
("scheduled task ran, nothing showed on Lark") is a special case of the missing
outbound path and is fixed by §4.

### Locked decisions

1. **Authority = human-like membership.** Channel posting requires the agent to be a
   member of that channel. DMs are allowed with **anyone the agent can see**. This
   *replaces* today's "agents are org-global, bypass channel membership."
2. **Addressing = extend `send_message`** with an optional destination (no new verb).
3. **Design the whole capability first** (this document), then implement in stages.

## Today's constraints (why prompt alone can't do this)

- `send_message` only ever posts to `ctx.thread_id`
  (`tools/system/send_message.rs:225`). `receiver` picks a colleague *within* that
  thread — there is no channel/DM/new-thread target.
- Agent context shows only `<colleagues>` (`memory/agent.rs`, `colleagues/render.rs`).
  No `<channels>`, no list of DMs. The agent cannot *name* a destination.
- Agents are **org-global**: humans are channel-gated via `is_channel_member`
  (user-keyed), agents skip it (`threads/traits.rs:163-164`,
  `send_message.rs:287-314`).
- Outbound to Lark/Slack is **inbound-bound**: a per-thread pump is attached only by
  the inbound bridge (`lark/bridge.rs:320`, `slack/bridge.rs`). Any
  proactively-created thread (scheduler / agent-opened DM) has no pump ⇒ silent.
- `schedule_task` captures only the *current* thread's channel
  (`schedule_task.rs target_channel`) — "from a DM, schedule on channel X" is not
  expressible.

Good news already in place: `lark_channels` maps Patom `channel_id` ↔ Lark `chat_id`
(+ `tenant_key`, `org_id`). The channel-level binding for outbound exists; we just
don't use it.

## Design

### 1. Permission model — agents become channel members

Membership today is **user-keyed** (`channel_members(user_id)`), and
`is_channel_member` takes a `user_id`. We make membership **colleague-keyed** so it
covers humans and agents uniformly (a colleague is already human-or-agent).

- New/extended membership keyed by `colleague_id` (humans keep their existing rows;
  agents get rows too). Add a query `channels_for_colleague(colleague) -> [Channel]`
  and `is_channel_member_colleague(thread|channel, colleague) -> bool`.
- **How agents gain membership:**
  - **Lark-backed channels:** the bot's presence in a Lark chat *is* the membership.
    Extend roster sync (`lark/roster.rs sync_on_join`, `BotAdded`) so when the bot
    joins a Lark chat that mirrors a Patom channel, the bot's **agent colleague** is
    written as a member of that channel. "Bot in Lark chat X" ⇒ "agent member of
    channel X." Removal on `BotRemoved`.
  - **Native Patom channels:** admins add agents via the existing channel/colleague
    admin surface (out of scope for v1 beyond the membership write existing).
- **Enforcement:** destination resolution in `send_message` checks agent membership
  for a **channel** target; **DM** targets are allowed with any visible colleague
  (per locked decision 1). The current-thread path keeps working because the agent
  is, by construction, a participant of the thread it runs in (resolve membership at
  thread-creation/participation time).

Migration: backfill agent membership for existing Lark-mirrored channels from the
bot's chat presence (`lark_channels` ⋈ current bot memberships). Reversible down.

### 2. Visibility — what the agent sees

Add a `<channels>` block to the agent turn context (`memory/agent.rs` assembly,
sibling to `colleagues/render.rs`):

- lists the channels the agent is a **member** of: `- <name> — channel, id <uuid>`,
  optionally annotated `(Lark)` / `(Slack)` / native.
- bounded like the roster (inline under a cap, else a `search`/`list` pointer).

`<colleagues>` already enumerates DM-able people/agents. Optionally annotate the
agent's *current location* (this thread's channel/DM) so it knows its default.

### 3. Addressing — `send_message` gains `to`

`SendMessageInput` gains an optional `to` destination (a tagged sum). `receiver`
keeps its current meaning *within the resolved thread* (the `@`-ping / agent
wake-up).

```jsonc
// to omitted        -> current thread (ctx.thread_id). Back-compat default.
{ "to": { "channel": "<channel_id>" } }       // start a NEW thread in that channel
{ "to": { "dm": "<human_colleague_id>" } }    // resolve-or-create the 1:1 DM thread (HUMAN only)
// (deferred) { "to": { "thread": "<thread_id>" } } // post into a specific existing thread
```

Resolution (new helper `resolve_or_create_target`):

1. **omitted** → `ctx.thread_id` (today's path, unchanged).
2. **channel** → check agent membership (reject if not a member); `create_thread`
   with `channel_id = Some`, `dm_counterpart = None`. **Each channel post is a new
   top-level thread** (locked v1). Continuing a channel conversation = post in the
   current thread. Targeting a specific existing thread by id is **deferred**.
3. **dm** → the target colleague **must be a human** (locked v1); an agent `dm`
   target is rejected — agent↔agent traffic stays in shared channels/threads via the
   current-thread `receiver` path. Resolve-or-create the DM thread between the agent
   and the human (`create_thread` `channel_id = None`, `dm_counterpart = colleague`).

After resolution: `resolve_participation(agent, thread)`, then the existing post +
`receiver` handling. **Then call the outbound router (§4) for the resolved thread.**

Care items: DAG budget / ping-pong guard when a turn spawns a *new* thread;
idempotency key for the created thread + posted row; `to`-self and cross-org guards.

### 4. Outbound delivery — binding-driven, decoupled from inbound

Replace "inbound attaches a pump" with "**any** trigger source ensures delivery for
its thread, resolved from bindings."

Introduce a core seam (no Lark/Slack dependency in `tools`/`scheduling`):

```rust
// patom-core, wired at the composition root to a Lark+Slack-aware impl.
trait OutboundRouter: Send + Sync {
    async fn ensure_delivery(&self, org_id: OrgId, thread_id: ThreadId);
}
```

`ensure_delivery` resolves the thread's external destination and starts/attaches the
right surface pump (idempotent; no-op for web-only threads):

- **Channel thread** → `lark_channels` (channel_id → chat_id, tenant_key) +
  `lark_apps` (org+agent → app_id/token). Post top-level (new Lark thread) or thread
  reply when continuing. (Slack analogue via `slack` channel map.)
- **DM thread** → see §5.
- **Inbound-originated thread** → existing `lark_threads` per-thread binding still
  used so replies thread correctly.

Callers of `ensure_delivery`:

- `send_message` (after resolving/creating its target thread),
- the **scheduler** (after `initiate_thread`) — **this fixes the original bug**,
- the inbound bridge (replaces its direct `stream_pump.attach`, or keeps it and also
  registers the binding).

The pump body itself (`lark/stream_pump.rs`) is reused; what changes is that
`AttachRequest` is built from a **binding lookup** keyed on the thread, callable from
anywhere — not only from inbound where `chat_id`/`app_id` happened to be in hand.

### 5. DMs to a person on Lark

A never-seen Lark DM has no `chat_id`. Lark lets you send to a user **directly by
`open_id`** (`im/v1/messages?receive_id_type=open_id`) — no pre-created chat needed.

- Extend the poster to send by `open_id` (resolved from the directory shadow of the
  recipient colleague).
- Capture the returned `chat_id` and persist a **DM binding** (Patom DM thread ↔
  Lark p2p `chat_id` + app_id) so subsequent turns thread/poll correctly.
- Recipient must be a Lark shadow with a known `open_id`; otherwise the DM stays
  web-only (still delivered in-app).

### 6. `schedule_task` gains a destination

Extend `schedule_task` to accept the same `to` shape (channel / dm), defaulting to
the current thread's channel (today's behavior). Persist on `scheduled_tasks`
(already has `channel_id`; add a DM counterpart / destination encoding). At fire
time the scheduler resolves-or-creates the thread for that destination and calls
`ensure_delivery`. This makes "from this DM, post to channel X daily" expressible.

### 7. Prompt / context updates

- `send_message` description: document `to` (current thread / channel you belong to /
  DM a person), with "choose the most appropriate place, like a colleague would."
- `<core>` guidance: you may reply here, start a thread in a channel you're a member
  of, or DM a person directly; only places you have access to.
- `schedule_task` description: document the destination argument.

## Staged TDD plan (failing test first, per CLAUDE.md §3)

1. **Membership model** — colleague-keyed channel membership: schema (+reversible
   down + backfill), store queries, `is_channel_member_colleague`. Lark roster wires
   bot→agent membership on join/leave.
2. **Channel visibility** — `channels_for_colleague` + `<channels>` context block.
3. **Outbound router seam** — `OutboundRouter` in core + Lark impl (binding-driven
   `ensure_delivery`); migrate scheduler + inbound bridge to it. *Fixes the
   scheduled-delivery bug.* (Slack impl follows the same seam.)
4. **DM outbound by open_id** — poster send-by-open_id + DM binding capture.
5. **`send_message` `to`** — `resolve_or_create_target` + membership gate + DAG/idem
   care; calls `ensure_delivery`.
6. **`schedule_task` destination** — argument + storage + scheduler resolves it.
7. **Prompt/context + e2e** — descriptions, `<core>`/`<channels>` wording, end-to-end
   on the changed surfaces.

## Resolved (locked v1)

- **Channel post = always a new top-level thread.** No `{ thread: id }` addressing in
  v1 (deferred). Continuing a conversation = post in the current thread.
- **DM authority = humans only.** Agent↔agent stays in shared channels/threads
  (current-thread `receiver` path); an agent `dm` target is rejected.

## Open questions

- **Native (non-Lark) channel agent membership** — admin assignment UX is out of
  scope here; only the membership write/enforcement is in.
- **Slack parity** — design is surface-agnostic; Slack impl of the seam is a parallel
  follow-up, not blocking the Lark path.
