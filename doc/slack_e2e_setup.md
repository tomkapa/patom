# Slack adapter — manual end-to-end setup

How to wire Phase 1 to a real Slack workspace and smoke-test the full
flow: mention → bridge → queue → worker → `chat.postMessage` reply.

Prereqs:
- Relay running locally (`cargo run`) on `http://localhost:8080`.
- A Slack workspace where you can install apps (any free workspace
  works; you do not need admin rights on your main org).
- A public HTTPS tunnel into your laptop so Slack can reach
  `/slack/events`. `ngrok http 8080` or `cloudflared tunnel run` —
  either is fine.

The 8-step checklist below assumes you already have a working Relay
dev environment (Postgres + Google OAuth login + at least one agent).

---

## 1. Start a tunnel

Slack's Events API needs a publicly reachable HTTPS URL. Start the
tunnel **before** the Slack app is configured so the URL is stable
across the next few steps.

```bash
ngrok http 8080
# → Forwarding https://abc123.ngrok.app -> http://localhost:8080
```

Keep this terminal open. Note the `https://abc123.ngrok.app` URL — it
goes into both your Slack app config and into Relay's
`RELAY_OAUTH_REDIRECT_BASE` env var.

> ⚠️ Free ngrok URLs change every restart. Plan to redo step 4 if
> you have to restart the tunnel mid-test.

---

## 2. Create the Slack app

1. Open <https://api.slack.com/apps> → **Create New App** → **From
   scratch**.
2. Name: `relay-dev` (anything). Workspace: pick a personal/test one.
3. After creation you land on **Basic Information**. Copy the
   **Signing Secret** — this is `RELAY_SLACK_SIGNING_SECRET`.
4. Scroll to **App-Level Tokens** — not used in Phase 1, skip.

---

## 3. Configure scopes + event subscription

Still inside the Slack app config:

**OAuth & Permissions** (left sidebar):

- **Bot Token Scopes** — add all three:
  - `app_mentions:read`
  - `chat:write`
  - `chat:write.customize` ← required for per-agent `username` override
- **Redirect URLs** — add `https://abc123.ngrok.app/slack/oauth/callback`
  (use your tunnel URL). Save.

**Event Subscriptions** (left sidebar):

- Toggle **Enable Events** → ON.
- **Request URL**: `https://abc123.ngrok.app/slack/events`
- Slack will immediately ping the URL with a `url_verification`
  challenge. **Wait for the green "Verified" check.** If you get a
  red error here, Relay isn't reachable through the tunnel or the
  signing secret in `.env` is wrong — fix and click **Retry**.
- **Subscribe to bot events** → add `app_mention`. Save.

**Basic Information** → grab the **Client ID** and **Client Secret** —
these are `RELAY_SLACK_CLIENT_ID` and `RELAY_SLACK_CLIENT_SECRET`.

---

## 4. Wire Relay's env vars

Edit your `.env` (copy `.env.example` if you haven't):

```bash
# Existing keys you should already have:
RELAY_OAUTH_REDIRECT_BASE=https://abc123.ngrok.app   # ← match the tunnel URL
# (others unchanged — JWT secret, master KEK, Google OAuth, Postgres, …)

# New Slack keys:
RELAY_SLACK_SIGNING_SECRET=<from step 2.3>
RELAY_SLACK_CLIENT_ID=<from step 3>
RELAY_SLACK_CLIENT_SECRET=<from step 3>
```

> The redirect URL Relay sends to Slack is built as
> `<RELAY_OAUTH_REDIRECT_BASE>/slack/oauth/callback`. Slack rejects
> the install if this exact string isn't on the **Redirect URLs**
> allowlist from step 3. Match the host case-sensitively.

Restart Relay (`cargo run`) so it picks the new env up. On startup
you should see normal logs — there's no "slack enabled" line, but
`curl https://abc123.ngrok.app/slack/events` (no headers) should
return `401 Unauthorized` (signature verification failing on an
empty body), proving the route is mounted. A 404 means the env vars
aren't taking — check `RELAY_SLACK_*` are spelled right.

---

## 5. Install the bot into your workspace

Sign into Relay's web UI (Google OAuth as usual). Then:

```bash
curl -X POST \
  -H "Cookie: relay_session=$(cat /tmp/relay_cookie)" \
  https://abc123.ngrok.app/api/slack/install
# → {"authorize_url": "https://slack.com/oauth/v2/authorize?…"}
```

> Phase 2 ships a settings-page button. For now you cookie-paste the
> session into curl. Grab the cookie from your browser's devtools
> (`relay_session=…`).

Open `authorize_url` in a browser, click **Allow** on Slack's consent
screen. Slack redirects to
`https://abc123.ngrok.app/slack/oauth/callback?code=…&state=…`.
Relay exchanges the code, seals the bot token via `OrgEncryptor`, and
inserts a `slack_workspaces` row.

You land on `/settings/slack` (the FE route doesn't exist yet — you
get a 404, that's fine; the install succeeded). Confirm:

```sql
psql $DATABASE_URL -c "SELECT org_id, team_id, team_name, installed_at
                       FROM slack_workspaces;"
```

One row should be visible.

---

## 6. Invite the bot to a channel

In your Slack workspace pick a test channel (or DM yourself). Type:

```
/invite @relay-dev
```

Slack will offer the bot in the dropdown. Send. The bot now sees
mentions in this channel.

---

## 7. Send a mention

In the same channel:

```
@relay-dev @<your-agent-name> what's the weather?
```

Replace `<your-agent-name>` with the `name` of any agent in your org
(case-insensitive). If your dev org has the seeded default
`recruiter`, that name works.

Expected timing:
- **<1s**: Slack acks the webhook (no UI signal — that's the 3-second
  budget being honoured).
- **~2–10s**: the worker picks up the prompt, runs the turn,
  publishes a `Done` chunk on `PgThreadStream`.
- **immediately after**: a reply appears in the thread, posted by
  `@relay-dev` but with `username` overridden to your agent's name.

If nothing appears in 30s, check:

```bash
# Relay logs — look for these spans:
grep -E "slack\.events|slack\.bridge|slack\.stream_pump" relay.log
```

Common failures:
- `slack.events.verify_failed` → signing secret mismatch.
- `slack.bridge.process_failed` with `UnknownWorkspace` → step 5
  didn't insert the row, or the wrong `team_id` was captured.
- `slack.stream_pump.post_failed` → bot lacks `chat:write.customize`
  scope, or the bot isn't a member of the channel (re-do step 6).
- No `slack.events.enqueued` at all → the webhook never reached you;
  check ngrok tunnel output and Slack's **Event Subscriptions** page
  shows recent successful deliveries.

---

## 8. Test continuation and cross-agent handoff

**Continuation** (same thread, same agent):

Reply in the *same* Slack thread (use the "Reply in thread" affordance,
not a new message):

```
@relay-dev tell me more
```

Expected: the same agent replies. Phase 1 binds the thread to its
original agent; tagging a different agent in a reply is ignored (the
mention is still parsed, but `process_event` reads the session's
existing participant). This matches the web UI's HTTP behaviour.

**Cross-agent handoff**:

Configure an agent that calls `send_message` to another agent (the
existing `Ask` flow). In a new Slack thread, mention that agent. When
it asks its peer, both messages surface in the same Slack thread —
the first attributed to the primary agent, the handoff attributed to
the second.

---

## Quick verification queries

```sql
-- Workspace install
SELECT org_id, team_id, team_name, installed_at FROM slack_workspaces;

-- Active thread bindings
SELECT team_id, channel_id, thread_ts, root_request_id, created_at
FROM slack_threads ORDER BY created_at DESC LIMIT 10;

-- Identity overrides (Phase 2; empty in Phase 1)
SELECT * FROM slack_identities;
```

---

## Reset / teardown

To wipe the install and start over:

```sql
-- ON DELETE CASCADE on slack_workspaces cleans identities + threads.
DELETE FROM slack_workspaces WHERE team_id = 'T0XXXXXX';
```

To remove the Slack app entirely: open the app at
<https://api.slack.com/apps>, scroll to the bottom, **Delete App**.
Workspace install is removed; Relay's `slack_workspaces` row remains
(orphaned but harmless) — run the DELETE above.
