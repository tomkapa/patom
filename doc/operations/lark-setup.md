# Lark (Feishu) adapter — manual end-to-end setup

How to wire the Lark BYO-bot integration to a real Lark tenant and
smoke-test the full flow: `@mention` / DM → bridge → queue → worker →
`im/v1/messages` reply, plus ambient (non-mention) group ingest.

**The big difference from Slack: no tunnel, no public URL.** Lark's
**long-connection** transport is *outbound* — Patom dials Lark over
`wss://` and events stream down that socket. So this works entirely on
`localhost` behind NAT; there is no webhook to expose.

Prereqs:
- Patom running locally (`cargo run --bin patom`) on `http://localhost:8080`.
- A Lark tenant where you can create a **self-built app** and an admin who
  can approve its release. A free [Lark](https://www.larksuite.com/) tenant
  works.
- A working Patom dev environment: Postgres + Google OAuth login + **at
  least one agent** (you need its id to map the bot).

> One Lark app = one bot = one agent. Register N apps for N agents.

---

## 1. Create the self-built app

1. Open the **Developer Console**: <https://open.larksuite.com/app>
   (international) or <https://open.feishu.cn/app> (CN). Use the same
   region your tenant is on.
2. **Create custom app** → name it `patom-dev` (anything), pick an icon.
3. On the app's **Credentials & Basic Info** page, copy the **App ID**
   (`cli_…`) and **App Secret** — you'll register these with Patom in
   step 6. The secret is shown once; reveal + copy it now.

---

## 2. Enable the Bot + add scopes

**Add features** (or **Features** → **Bot**) → enable **Bot**.

**Permissions & Scopes** (left sidebar) → add these four (search by the
identifier, or use **Add by API** and pick the endpoint):

| Scope identifier | What it grants |
|---|---|
| `im:message` | Send replies **and** read chat history (`im/v1/messages`). |
| `im:message.p2p_msg:readonly` | **Receive DM (1:1) messages.** Required to get the `im.message.receive_v1` event for a direct chat with the bot — without it, DMing the bot does *nothing*. |
| `im:message.group_msg` | **Receive all group messages** (mentions + non-`@` ambient ingest). Without it the bot gets no group events (or only `@`-mentions if you instead use `im:message.group_at_msg:readonly`). |
| `im:chat:readonly` | List chat members (roster-on-join). |
| `contact:user.employee_id:readonly` | Resolve `user_id` (employee_id) — the **identity key**. **Hard requirement**: without it events/roster omit `user_id` and senders are dropped. |

> Receiving the message **event** is gated by the `*_msg` scopes above
> (`im:message.p2p_msg:readonly` for DMs, `im:message.group_msg` for groups) —
> `im:message` alone grants the *send/read API* but does **not** deliver
> inbound events. `im:message.history:readonly` is optional (covered by
> `im:message`).
>
> ⚠️ **Re-publish after any scope change** (step 4) — added scopes don't take
> effect until a new app version is released and admin-approved.

---

## 3. Events → long-connection mode

**Events & callbacks** (or **Event Subscriptions**) (left sidebar):

1. **Subscription method** → choose **Use long connection** (长连接) — NOT
   "Send to URL". This is what lets Patom dial out; no Request URL is
   needed.

   > ⚠️ **Go/no-go:** confirm "Use long connection" is actually selectable
   > on a real `larksuite.com` (international) self-built app — its console
   > exposure has been inconsistent. If the option is missing, the live
   > path can't connect (and the adapter has no webhook fallback yet).

2. **Add events** → subscribe:
   - `im.message.receive_v1` — inbound messages (mentions, DMs, ambient).
   - `im.chat.member.bot.added_v1` — bot added to a chat (roster sync).
   - `im.chat.member.user.added_v1` / `im.chat.member.user.deleted_v1` —
     roster freshness.
   - `p2p_chat_create` — first-DM-with-the-bot (legacy schema-1.0 event;
     it lives under the old event list, not the `*_v1` search).

3. Save.

---

## 4. Publish + admin approval

**Version Management & Release** (left sidebar):

1. **Create a version** → fill the required fields → **submit for
   release**.
2. A **tenant admin must approve** it. Until the version is released,
   `tenant_access_token` mints fail and the bot can't act.

---

## 5. Wire Patom's env vars

Edit your `.env` (copy `.env.example` if you haven't):

```bash
PATOM_LARK_ENABLED=true
PATOM_LARK_API_BASE=https://open.larksuite.com   # CN: https://open.feishu.cn
# (everything else unchanged — JWT secret, master KEK, Google OAuth, Postgres, …)
```

Per-bot credentials are **not** in env — they're registered per-org in the
DB (encrypted) in the next step. Restart Patom (`cargo run --bin patom`) so
it picks up `PATOM_LARK_*`. With the flag off, `/api/lark/apps` 404s and the
WS manager never spawns.

---

## 6. Register the bot → agent

Get the **agent id** you want the bot to speak as — from the Patom web UI
(Agents page → the agent's id) or `GET /api/agents`.

Register the app (admin route). This is a **state-changing POST in the
authenticated subtree**, so it needs both the login cookie **and** the
double-submit CSRF token. From browser devtools → **Application → Cookies**,
copy two values: `patom_session` (the login JWT) and `patom_csrf`
(non-HttpOnly). Pass `patom_csrf` as both the `X-CSRF-Token` header and the
cookie:

```bash
SESSION="<patom_session value>"
CSRF="<patom_csrf value>"

curl -i -X POST http://localhost:8080/api/lark/apps \
  -H 'content-type: application/json' \
  -H "X-CSRF-Token: $CSRF" \
  -H "Cookie: patom_session=$SESSION; patom_csrf=$CSRF" \
  -d '{"app_id":"cli_xxx","app_secret":"yyy","agent_id":"<patom-agent-uuid>"}'
# → HTTP/1.1 201 Created
```

`-i` prints the status line — a silent empty response usually means a wrong
status you couldn't see (`401` bad/expired cookie · `403` missing/mismatched
CSRF token · `404` Lark disabled, i.e. `PATOM_LARK_ENABLED` unset or the server
not restarted after enabling it).

Other admin verbs: `GET /api/lark/apps` (list — a *safe* method, needs only
`patom_session`, no CSRF token), `DELETE /api/lark/apps/{app_id}` (needs the
CSRF token like POST).

The app secret is sealed via `OrgEncryptor`. Confirm:

```sql
psql "$DATABASE_URL" -c "SELECT org_id, app_id, agent_id, created_at FROM lark_apps;"
```

The bot **connects immediately on registration** — `POST /api/lark/apps` hot-adds
it to the running WS manager (no restart needed). In the `cargo run` console (the
default filter is `patom=info`, so no `RUST_LOG` needed) you should see, within a
second of the `curl`:

```
lark.ws.connected app=cli_… service_id=…   # the long-connection is up
```

At startup the manager also sweeps every already-registered bot:

```
lark.ws.manager_start count=N              # N registered bots found + connected
```

If you see `lark.ws.manager_start` but **no** `lark.ws.connected` (and a
`lark.ws.connection_error`), the handshake/dial failed — bad app_id/secret, an
unreleased/un-approved version (step 4), or long-connection isn't actually
available on this console.

---

## 7. Add the bot to a group (or DM it)

In your Lark tenant, open a group chat → **Settings** → **Bots** → **Add**
→ pick `patom-dev`. (Or just open a 1:1 chat with the bot.)

Adding the bot fires `im.chat.member.bot.added_v1` → Patom syncs the roster
and shadow-mints a colleague for every member (so the agent knows everyone,
even silent ones).

---

## 8. Send a mention / DM

**In the group**, `@`-mention the bot:

```text
@patom-dev draft a job description
```

**Or DM the bot** directly (DMs always trigger — no mention needed):

```text
draft a job description
```

Expected timing:
- **<3s**: the WS frame is ACKed (`{"code":200}`) — no UI signal; that's
  Lark's deadline being honoured, the agent runs off the socket.
- **~2–10s**: the worker runs the turn and publishes a reply on
  `PgThreadStream`.
- **immediately after**: the agent's reply posts back in the chat/thread as
  the bot.

Log spans to watch:

```bash
grep -E "lark\.ws|lark\.bridge|lark\.stream_pump" patom.log
```

A healthy mention shows `lark.bridge.enqueued`.

Common failures:
- **No `lark.ws.manager_start`** → `PATOM_LARK_ENABLED` not set (the server
  needs a restart after enabling it). A bot registered later hot-connects
  (`lark.ws.connected`) without a `manager_start`.
- **`lark.ws.connection_error`** → handshake failed: wrong app_id/secret,
  the version isn't released/approved (step 4), or long-connection isn't
  available on this (international) console.
- **`lark.ws.bot_open_id_token_failed`** → `tenant_access_token` mint
  failed — bad credentials or unreleased app.
- **`lark.bridge.sender_missing_user_id_dropped`** → the
  `contact:user.employee_id:readonly` scope is missing; events carry no
  `user_id`, so senders can't be keyed and are dropped.
- **`lark.ws.connected` shows but a message produces NO `lark.bridge.*` log**
  → the event isn't being delivered: you're missing the *receive* scope
  (**`im:message.p2p_msg:readonly`** for a DM, **`im:message.group_msg`** for a
  group), the event isn't subscribed, or — most common — you added the scope
  but **didn't re-publish + re-approve** the app version (step 4). Scope
  changes are inert until the new version is released.
- **Bot never receives non-mention group messages** → missing
  `im:message.group_msg` (only `@`-mentions arrive without it).
- **`lark.stream_pump.post_failed`** → missing send scope, or the bot isn't
  in the chat.
- **Reply appears but the worker then stalls / re-leases the turn** → the
  known open issue below.

---

## 9. Ambient ingest (no reply expected)

Have **another person** post a plain message in the group **without**
mentioning the bot:

```text
fyi the deadline moved to Friday
```

Expected: **no reply** (it's not a trigger), but the message is **ingested**
— a shadow colleague is minted for the sender and the message is appended to
the Patom thread. On the *next* mention, the agent's context includes it
(it "saw" the whole conversation). Verify via the queries below.

---

## ⚠️ Known issue (live test will hit this)

The mention/DM **flow is verified** end-to-end (shadow mint → thread →
trigger → worker → agent → reply posts back), but in the integration test
the worker pool does **not** terminate cleanly after a turn whose acting
user is a *shadow* (login-less) Lark user — unlike Slack's real linked user.
In production this could surface as a Lark-triggered turn that posts its
reply but then the worker gets stuck / re-leases. **Your live run is the
signal we need**: if the agent replies but the worker keeps spinning on the
same request, that's this issue (turn finalization for a login-less acting
user), not a config problem. Report it and we'll fix the lifecycle.

---

## Quick verification queries

`lark_*` tables enforce `FORCE ROW LEVEL SECURITY`, so read them with the
DB owner via `SET row_security = off` (or just query as the migration role):

```sql
-- Registered bots
SELECT org_id, app_id, agent_id, tenant_key, created_at FROM lark_apps;

-- The people directory (one shadow per Lark user per tenant)
SELECT tenant_key, lark_user_id, open_id, colleague_id FROM lark_user_handles;

-- Lark-thread ↔ Patom-thread bindings
SELECT tenant_key, chat_id, lark_thread_id, patom_thread_id, created_at
FROM lark_threads ORDER BY created_at DESC LIMIT 10;

-- Mirrored Lark chats → Patom channels
SELECT tenant_key, chat_id, channel_id, created_at FROM lark_channels;

-- Triggers enqueued by the bridge (idempotency_key = lark:<tenant>:<event_id>)
SELECT idempotency_key, created_at FROM prompt_requests
WHERE idempotency_key LIKE 'lark:%' ORDER BY created_at DESC LIMIT 10;
```

---

## Reset / teardown

To wipe a bot registration and start over (cascades clean up
`lark_threads`; `lark_user_handles` / `lark_channels` reference
`organizations` / `colleagues`, not the app, so they persist as harmless
history):

```sql
DELETE FROM lark_apps WHERE app_id = 'cli_xxx';
```

To remove the Lark app entirely: in the Developer Console, delete the app
(or unpublish the version). The long-connection drops; Patom's `lark_apps`
row remains (orphaned but harmless) — run the DELETE above.

---

## Local automated test (no tenant needed)

```bash
docker compose up -d postgres
cargo test -p patom-core --lib lark::      # ~70 unit tests (codec, handshake, token, mention, poster, ws-manager)
cargo test -p patom-core --test lark_e2e   # ambient-ingest e2e (clean)
# the mention→reply e2e is #[ignore]d (flow verified; worker-teardown hang — see the known issue):
#   cargo test -p patom-core --test lark_e2e -- --ignored --nocapture dm_message_drives   # watch, then Ctrl-C
```
