# Lark + Discord: render agent MCP connection requests in chat (#181)

Status: PLANNED (2026-06-17). Scope: **Lark + Discord** (issue is Lark-only; Discord
folded in for parity — same gap, same fix shape).

## Problem

Agents emit `ResponseChunk::WireMcpRequest { from, catalog_id, display_name, reason,
auth_kind, homepage_url }` (`runtime/response.rs:75`) via the `request_user_wire_mcp`
tool. Slack renders this as a Block Kit card with a signed connect link; **Lark and
Discord drop it** — their stream-pump `render_payload` matches only
`Done`/`AgentMessage`/`Error` and `WireMcpRequest` falls through to `_ => None`.

- Lark: `lark/stream_pump.rs:372` `render_payload`
- Discord: `discord/stream_pump.rs:316` `render_payload`

Both transports are **text-only** (no Block Kit, no Discord components in the poster —
`discord/poster.rs:80` `PostRequest` carries `content` + `allowed_mentions` only;
`lark/poster.rs:47` carries `text`). So both render as a **plain-text message with a
URL**, not a card. (Discord *could* technically send link-style components, but the
poster has no component path and the long-connection/Gateway constraint mirrors Lark —
keep it plain-text for both, consistent with the issue's Lark constraint.)

## Reference: how Slack works (the blueprint)

1. `slack/stream_pump.rs` — on `WireMcpRequest` + `auth_kind == OAuth2`, calls
   `build_connect_url` (`:418`) to mint a signed URL, renders via `payload_for_post`
   (`:372`), and **defers** the card into a `Vec<DeferredPost>` (cap
   `MAX_DEFERRED_WIRE_CARDS = 8`) so it posts *after* the accompanying text (narrative
   order). Non-OAuth kinds get a "finish in the web UI" hint instead of a button.
2. `slack/connect_link.rs` — `SlackConnectClaims { catalog_id, team_id, channel_id,
   thread_ts, slack_user_id, thread_id, agent_id }`, HMAC-SHA256 signed, colon-delimited
   payload + `:exp:hex_sig`, 10-min TTL (`CONNECT_LINK_TTL_SECS`). `sign_connect` /
   `verify_connect` (constant-time `ct_eq`).
3. `http/routes/mcp.rs` — `GET /slack/mcp/connect?token=…` (`slack_connect_router` `:82`):
   verify token → resolve platform user → `(user_id, org_id)` → `install_from_catalog`
   (`:599`, idempotent) → `slack_connect_build_oauth_start` writes the pending row →
   redirect to provider authorize URL.
4. OAuth callback (`mcp.rs:1372` `handle_oauth_callback` → `callback_flow`): reads the
   pending ctx, marks connected, then `tokio::join!(do_auto_continue, do_slack_ping)`.

## Key seams discovered

### Resume is already platform-agnostic — reuse for free
`ResumeCtx { thread_id, agent_id }` (`mcp/oauth/state_adapter.rs:44`) + `do_auto_continue`
(`mcp.rs:1600`) drive `submit_internal` to re-trigger the agent. **No platform branching.**
Lark/Discord populate `resume_ctx` and the agent resumes with zero new resume code.

### The success "ping" IS Slack-specific — needs extension
`PatomPendingCtx` (`state_adapter.rs:70`) carries `slack_ctx: Option<SlackPingCtx>`;
`do_slack_ping` (`mcp.rs:1658`) is dispatched on `slack_ctx.is_some()`. We add
`lark_ctx` / `discord_ctx` + `do_lark_ping` / `do_discord_ping`. The pending row is in
table `mcp_oauth_pending` with **all-or-none CHECK constraints** per context group
(migration 34, renamed 64). Adding columns ⇒ **migration 86** + matching CHECK + matching
validation in `PendingCtxRow::into_ctx` (`state_adapter.rs:284`).

### Platform user id is available at AttachRequest build time, but not threaded
- Slack `AttachRequest` carries `slack_user_id`; Lark/Discord do **not**.
- Lark: built in `lark/bridge.rs:434` `enqueue_and_attach`; `caller.user_id` (the
  shadow-minted Patom user) and `m.sender_user_id`/`tenant_key` are in scope.
- Discord: built in `discord/bridge.rs:792` `enqueue_and_attach`; `caller.user_id`
  (shadow-minted) and `m.author.id` are in scope.
- Lark/Discord have **no link table** like `slack_identities` — they shadow-mint
  (`lark_user_handles` / `discord_user_handles`) one-way at inbound. So we already hold
  the resolved Patom `user_id` + `org_id` at attach time.

**Design decision — carry resolved `(user_id, org_id)` in the signed claims directly**
rather than a platform user id we'd re-resolve at connect time. Rationale: (a) the value
is already resolved at inbound (shadow-mint), so a reverse `*_user_handles` lookup at
connect time is redundant; (b) the token is HMAC-signed so the embedded ids are not
attacker-controlled; (c) avoids a Lark/Discord analogue of
`resolve_slack_connect_identity`. We still embed `tenant_key`+`chat_id`(+`thread`) /
`guild_id`+`channel_id` because the **ping-back** needs them to post the "✓ Connected"
message. (Slack re-resolves because its link phase is genuinely two-step; ours isn't.)

This means `AttachRequest` for Lark/Discord must additionally carry `org_id` (Lark already
has it; Discord already has it) and the resolved `user_id` — thread `caller.user_id` into
the attach site. The platform user id itself is **not** needed in claims (it's display-only).

## Plan (TDD, gates green per CLAUDE.md §3)

Lark and Discord are near-identical; build Lark first, then mirror to Discord. Each step
is red→green→refactor.

### Phase 0 — shared OAuth-callback ping plumbing (migration 86)
1. `LarkPingCtx { tenant_key, chat_id, reply_to: Option<..> }` and
   `DiscordPingCtx { application_id, channel_id, reply_to: Option<..> }` in
   `state_adapter.rs`. Add `lark_ctx`/`discord_ctx: Option<..>` to `PatomPendingCtx`.
2. Migration 86: add nullable columns + all-or-none CHECK per group; paired down.
   Extend `save` INSERT, `read_pending_ctx` SELECT, and `PendingCtxRow::into_ctx`
   validation (match-on-all-Some, else `Misconfigured`). Test the round-trip.
3. `do_lark_ping` / `do_discord_ping` (mirror `do_slack_ping:1658`): post
   "✓ Connected — {display_name}" via the platform poster (needs token provider for
   Lark, bot token for Discord — resolve via app_store by the ctx ids). Add both to the
   `tokio::join!` at `mcp.rs:1553`. Each is best-effort, never fails the callback.

### Phase 1 — Lark connect link
4. `lark/connect_link.rs` (mirror `slack/connect_link.rs`): `LarkConnectClaims
   { catalog_id, tenant_key, chat_id, reply_to, org_id, user_id, thread_id, agent_id }`,
   `sign_connect`/`verify_connect`, HMAC-SHA256, `LARK_CONNECT_LINK_TTL_SECS` (600) in
   `lark/limits.rs`. **Test first**: sign→verify round-trip, tamper rejection, expiry,
   trailing-garbage rejection.

### Phase 2 — Lark stream-pump rendering
5. Thread `org_id` + resolved `user_id` (+ `tenant_key`) into Lark `AttachRequest`
   (`lark/stream_pump.rs:38`) at the `lark/bridge.rs:434` build site.
6. Add `WireMcpRequest` arm to `render_payload` + deferred buffering in the pump loop
   (mirror Slack's `Vec<DeferredPost>` cap `MAX_DEFERRED_WIRE_CARDS`, reuse or add to
   `lark/limits.rs`). Build a plain-text body:
   - OAuth2: `"🔌 {agent} wants to connect {display_name}: {reason}\nOpen Patom to
     connect: {url}"` where url = `{base}/lark/mcp/connect?token=…`.
   - StaticHeaders/None: same lead line + "Finish wiring {display_name} in the Patom web
     UI: {web_base}/…". Truncate reason to a `LARK_CONNECTION_REASON_MAX_CHARS`.
   **Test**: pump emits a post containing the signed URL for OAuth2; web-UI pointer for
   other kinds; deferred-after-text ordering.

### Phase 3 — Lark connect endpoint
7. `GET /lark/mcp/connect?token=…` in `http/routes/mcp.rs` (mirror `slack_connect_router`
   + `handle_slack_connect_inner`): verify token → use claims' `(user_id, org_id)`
   directly → `install_from_catalog` (reuse `:599`) → build OAuth start writing
   `resume_ctx` + `lark_ctx`. Register the router. **Test**: bad/expired token → error
   HTML; valid token → redirect to authorize URL + pending row carries lark_ctx+resume.

### Phase 4 — Discord (mirror Phases 1–3)
8. `discord/connect_link.rs`: `DiscordConnectClaims { catalog_id, application_id,
   channel_id (ContainerId), reply_to, org_id, user_id, thread_id, agent_id }`. TTL in
   `discord/limits.rs`.
9. Thread `org_id`+`user_id` into Discord `AttachRequest` (`discord/bridge.rs:792`).
   Add `WireMcpRequest` arm + deferred buffer to `discord/stream_pump.rs:316`. Plain-text
   body, url = `{base}/discord/mcp/connect?token=…`. 2000-char chunking already handled
   by poster.
10. `GET /discord/mcp/connect?token=…` handler. Register router.

### Phase 5 — gates
11. `fmt`, `clippy -D warnings`, `check`, `cargo nextest run`, e2e for changed surface.
    Coverage on the two new `connect_link.rs` (sign/verify) and the pump branches.

## Files touched

New:
- `crates/patom-core/src/lark/connect_link.rs`
- `crates/patom-core/src/discord/connect_link.rs`
- `crates/patom-core/migrations/00000000000086_mcp_oauth_pending_chat_ctx.{up,down}.sql`

Modified:
- `lark/stream_pump.rs`, `lark/bridge.rs`, `lark/limits.rs`, `lark/mod.rs`
- `discord/stream_pump.rs`, `discord/bridge.rs`, `discord/limits.rs`, `discord/mod.rs`
- `mcp/oauth/state_adapter.rs` (ping ctx structs + row mapping)
- `http/routes/mcp.rs` (two new connect handlers + routers + two `do_*_ping` + join)
- `http/routes/mod.rs` (register both routers)

## Risks / notes

- **Newtypes (CLAUDE.md §1):** claims fields are domain newtypes (`LarkTenantKey`,
  `LarkChatId`, `ContainerId`, `OrgId`, `UserId`, `ThreadId`, `AgentId`,
  `McpCatalogId`) — `verify_connect` parses each via `TryFrom`; no bare strings in the core.
- **Token holds `user_id`/`org_id`:** acceptable only because HMAC-signed and short-TTL.
  Document the trust boundary in the module header (mirror Slack's comment block).
- **Lark `reply_to`/Discord `reply_to` may be `None`** (top-level msg) — ping posts
  top-level then; fine. Keep the all-or-none CHECK over the *required* ctx ids only,
  leaving `reply_to` independently nullable, OR fold reply_to out of the ctx and re-post
  top-level. Simpler: ping posts top-level to the chat/channel (no reply threading needed
  for a confirmation). Recommend dropping `reply_to` from ping ctx to keep CHECK simple.
- **No Block Kit / components:** plain-text URL only. Acceptable per issue constraint.
- **DeepSeek/etc unaffected** — this is transport rendering, not model input.
```
