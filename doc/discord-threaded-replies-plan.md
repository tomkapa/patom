# Discord — agent replies in a thread (design, for a future session)

> Status: **planned, not implemented.** Follow-up to the Discord BYO-bot integration (PR #186, branch `feat/discord-byo-integration`). Authored after a live test showed the agent replying in the **channel** instead of a **thread**. The product decision (confirmed with the user) is **auto-open a thread**: when a member `@mentions` the agent at the channel top level, the bot opens a Discord thread on that message and the whole exchange lives there, keeping the channel clean.

## Current behavior (what ships in PR #186)

The agent's reply lands wherever the **triggering message** lives — because the bridge sets the outbound container to `m.channel_id` and the pump posts `POST /channels/{container}/messages`:

- `@mention` typed at the **channel top level** → `m.channel_id` = the channel → reply posts to the channel (the observed gap).
- `@mention` typed **inside an existing Discord thread** → `m.channel_id` = the thread id (a Discord thread *is* a channel) → reply already lands in that thread.
- **DM** → reply in the DM channel.

Reply uses `message_reference { message_id, fail_if_not_exists: false }`, so it shows as a Discord inline reply.

Relevant code:
- `crates/patom-core/src/discord/bridge.rs` — `handle_message` (identity → channel → thread → append → trigger), `resolve_thread`, `enqueue_and_attach` (builds `AttachRequest { container_id, reply_to, … }`).
- `crates/patom-core/src/discord/stream_pump.rs` — `post_reply` → `poster.post(PostRequest { container_id, reply_to, … })`.
- `crates/patom-core/src/discord/poster.rs` — `PostRequest.reply_to: Option<DiscordMessageId>` (already optional), `message_reference` wire body.
- `crates/patom-core/src/discord/thread_map.rs` — `discord_threads` binding `(guild_id, container_id) → patom_thread`, with `parent_id` (nullable) already a column.

## Target behavior

| Trigger location | Where the agent replies | Continuation |
|---|---|---|
| Top-level guild channel `@mention` | a **new thread** the bot opens on the triggering message | every message in that thread continues the conversation (no re-mention needed) |
| Already inside a **bot-opened** thread | that thread | same (auto-continues) |
| Already inside a **user-made** thread | that thread (reply when mentioned) | follow-ups need a re-mention (we don't own it) |
| DM | the DM channel | every DM message triggers (unchanged) |
| Thread-open fails (perms / already a thread / forum) | degrade to an **inline channel reply** | next `@mention` retries |

## Design

### 1. `is_thread` is derived, no new migration

A bound container is a thread iff `discord_threads.parent_id IS NOT NULL`. When the bot opens a thread `T` from a message in channel `C`, it binds the Patom thread to `T` with `parent_id = C`. Channel/DM conversations bind with `parent_id = NULL`. So:

```
is_thread(container) := exists a discord_threads row for it with parent_id NOT NULL
```

No `ALTER TABLE` needed — `parent_id` already exists (migration 81).

`DiscordThreadMapping` (`thread_map.rs`) gains `is_thread: bool`, computed in `lookup_by_container`:

```sql
SELECT patom_thread_id, backfill_complete, (parent_id IS NOT NULL) AS is_thread
  FROM discord_threads WHERE guild_id = $1 AND container_id = $2
```

### 2. Trigger semantics (revised)

```
is_trigger = m.guild_id.is_none()                 -- DM
          || m.mentions_bot(bot_user_id)          -- explicit @mention
          || in_bot_owned_thread                  -- a message inside a thread we own
```

where `in_bot_owned_thread = lookup_by_container(guild, m.channel_id).map_or(false, |b| b.is_thread)`.

This makes a bot-opened thread behave like a DM: every message there triggers a run. A **user-made** thread is *not* bot-owned (no binding, or `parent_id NULL`), so it only triggers on an explicit `@mention` — acceptable.

### 3. Resolving the conversation container

In `handle_message`, after resolving the sender shadow and computing `is_trigger`:

```
let existing = threads.lookup_by_container(guild, &m.channel_id).await?;
let in_owned_thread = existing.as_ref().is_some_and(|b| b.is_thread);

let (conv_container, parent, reply_to) =
    if m.guild_id.is_some() && is_trigger && !in_owned_thread {
        // Top-level channel mention → open a thread on the message.
        match thread_opener.open_from_message(&app.application_id, &m.channel_id, &m.message_id, &thread_name(&m)).await {
            Ok(t)  => (t, Some(m.channel_id.clone()), None),          // converse in the new thread
            Err(e) => { warn(e); (m.channel_id.clone(), None, Some(m.message_id.clone())) } // fallback: inline channel reply
        }
    } else if in_owned_thread {
        (m.channel_id.clone(), None, None)                           // continue in the thread (no message_reference)
    } else {
        // DM trigger, or ambient (no trigger anyway).
        (m.channel_id.clone(), None, is_trigger.then(|| m.message_id.clone()))
    };
```

Then:
- `ensure_channel(org, guild, &conv_container, shadow.user_id)` — Patom channel membership for RLS, keyed on the conversation container (one Patom channel per Discord thread/channel).
- `resolve_thread` keyed on **`conv_container`** (not `m.channel_id`), binding with `parent` when creating: `bind(org, app_id, guild, conv_container, parent.as_ref(), patom_thread)`.
- Mirror the triggering message into that Patom thread (it's the thread's logical first message).
- `AttachRequest { container_id: conv_container, reply_to, … }` (so the pump posts there).

`AttachRequest.reply_to` becomes `Option<DiscordMessageId>` (None inside a fresh/owned thread; `Some(trigger)` for channel/DM). `stream_pump::post_reply` already forwards an `Option` to `PostRequest.reply_to` — make the chain optional end-to-end.

### 4. Backfill interaction

A freshly-opened thread `T` has no pre-thread history, so **skip backfill when we just opened a thread** (`conv_container != m.channel_id`). Backfill still runs for channel/DM conversations and for continuation in an owned thread (its own `backfill_complete` gate already makes it a one-shot). `before` cursor stays `m.message_id`.

### 5. Thread name

Derive from the triggering message: the rendered content, trimmed + collapsed whitespace, truncated to `DISCORD_THREAD_NAME_MAX` (100) chars; fall back to a default (e.g. `"conversation"`) when empty. Optionally prefix with the agent name (needs an `agents.name` lookup via `app.agent_id` — optional polish).

## New code

- **`limits.rs`**: `DISCORD_THREAD_NAME_MAX = 100`, `DISCORD_THREAD_AUTO_ARCHIVE_MINUTES = 1_440`.
- **`thread_opener.rs`** (new seam, mirrors `history.rs`/`poster.rs` shape):
  ```rust
  #[async_trait]
  pub trait ThreadOpener: Debug + Send + Sync {
      async fn open_from_message(
          &self, application_id: &ApplicationId, channel_id: &ContainerId,
          message_id: &DiscordMessageId, name: &str,
      ) -> Result<ContainerId, DiscordError>;
  }
  pub type SharedThreadOpener = Arc<dyn ThreadOpener>;
  ```
  - `HttpDiscordThreadOpener` — `POST {api_base}/channels/{channel}/messages/{message}/threads`
    body `{ "name": <≤100>, "auto_archive_duration": 1440 }`, `Authorization: Bot …`,
    shares the `RateLimiter`, parses the returned channel object's `id` → `ContainerId`.
    Map non-2xx → `DiscordError::PostFailed` (the caller treats any `Err` as "can't thread → fall back").
  - `FakeThreadOpener` — returns a configured thread id (and/or records calls); a `failing()`
    variant that returns `Err` to exercise the fallback.
- **`thread_map.rs`**: add `is_thread` to `DiscordThreadMapping` + the `lookup_by_container` query (above). `bind` already takes `parent_id: Option<&ContainerId>`.
- **`bridge.rs`**: `BridgeDeps.thread_opener: SharedThreadOpener`; the `handle_message` restructure (§3); `resolve_thread` keyed on `conv_container` + `parent`; thread-name helper.
- **`app.rs`** discord block: build `HttpDiscordThreadOpener` (share the existing `limiter` + `discord_http` + `tokens`), inject into `BridgeDeps.thread_opener`.

## Tests to update / add

Existing (`tests/discord_bridge.rs`) — add `thread_opener` to the rig (a `FakeThreadOpener` returning a known thread id):
- `mention_enqueues_a_trigger_and_attaches_pump` → assert the `AttachRequest.container_id` is the **fake thread id** (not the channel), and a `discord_threads` row exists with `parent_id` set (`is_thread`).
- `dm_message_always_triggers` → unchanged target (DM channel), `reply_to = Some`.
- ambient / self-drop / redelivery → unaffected (opener never called on non-triggers).

New (`tests/discord_threads.rs`, `#[sqlx::test]`):
- **opens a thread on a channel mention** — process a guild `@mention`; assert a thread binding with `parent_id = channel`, the reply container = the opened thread, and a follow-up *without* a mention inside that thread still triggers (`is_trigger` via `in_owned_thread`).
- **continues in an existing owned thread** — second message in the thread reuses the same Patom thread, no second `open_from_message`.
- **fallback to channel on open failure** — `FakeThreadOpener::failing()` → reply container = the channel, `reply_to = Some(trigger)`, no thread binding created.
- **DM unaffected** — no thread opened.

`thread_opener.rs` in-module unit tests: parse the create-thread response → `ContainerId`; name truncation.

## Edge cases & notes

- **User-made thread + `@mention`**: `m.channel_id` is the thread; `open_from_message` returns `Err` (Discord 400 "cannot start a thread from a message in a thread") → fallback replies **in that thread** (correct). Follow-ups need a re-mention (we don't own it). Fine.
- **Forum channels** (`GuildForum`/`GuildMedia`): a "message" is itself a thread; `open_from_message` will error → fallback. Acceptable for the experiment; revisit if forum support is wanted.
- **Multiple separate channel mentions** → multiple threads (each mention = its own conversation). Matches the chosen UX.
- **`message_reference` inside a fresh thread**: drop it (the trigger message lives in the parent channel, not the thread, so a cross-channel reference can error). The thread context is implicit.
- **`ChannelType`** (`types.rs`) already has `is_thread()` / `is_dm()` helpers if richer detection is later wanted (e.g. consuming `THREAD_CREATE`, currently `DiscordEvent::Other`).

## Out of scope (leave deferred)

- Consuming `THREAD_CREATE`/`THREAD_UPDATE`/`THREAD_DELETE` to pre-populate thread→parent mappings (the derive-from-`parent_id` approach covers bot-owned threads without it).
- Naming threads with the agent's avatar/name styling.
