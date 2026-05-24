# Gmail / Calendar MCP — Preview-program gate and self-hosted workaround

## Why this doc exists

The Northstar pitch demo (`doc/pitch_demo.md`) wires Gmail and Google
Calendar via Google's hosted remote MCP servers
(`gmailmcp.googleapis.com`, `calendarmcp.googleapis.com`). We brought
the OAuth shared-client architecture online to drive them, completed
the consent flow successfully, and then hit a hard, opaque wall on the
actual tool invocations. This document records what we tried, what we
ruled out, what's actually gating us, and the workaround that
unblocks the demo without waiting on Google.

## Symptom

Once an org user finished the OAuth consent flow:

1. `GET https://gmail.googleapis.com/gmail/v1/users/me/labels`
   succeeded with the stored bearer — proving token + scopes + Gmail
   API enablement are all correct.
2. `POST gmailmcp.googleapis.com/mcp/v1` with `method: tools/list`
   succeeded — proving the bearer reaches the MCP gateway and the
   token is accepted for discovery.
3. `POST gmailmcp.googleapis.com/mcp/v1` with `method: tools/call`
   (any tool, any args) returned `{"isError": true, "content":[{
   "text":"The caller does not have permission"}]}`.

Calendar (`calendarmcp.googleapis.com`) behaved identically — same
opaque rejection on every `tools/call`, same success on `tools/list`.

## What we ruled out (with evidence)

- **Scope mismatch.** `tokeninfo` confirmed the token carried
  `gmail.compose gmail.readonly` (and the right Calendar scopes on
  the Calendar token). The exact scopes advertised by the MCP
  server's `scopes_supported`.
- **Project mismatch / Gmail API not enabled.** The direct
  `gmail.googleapis.com` call returned the user's labels, which only
  works when the calling project has Gmail API enabled and the token
  is valid for that project. Both APIs enabled and verified by the
  operator.
- **Restricted-scope verification gate.** Calendar's scopes are
  non-sensitive and Calendar `tools/call` *also* fails — so the gate
  isn't tied to Gmail's restricted-scope tier.
- **`X-Goog-User-Project` header missing.** Adding it (with the
  project number from the OAuth client_id) didn't change the
  outcome.
- **`prompt=consent` / `access_type=offline`.** Both added to the
  authorize URL ([src/mcp/oauth/flow.rs](../src/mcp/oauth/flow.rs)).
  Fresh tokens, refresh tokens issued, scopes granted explicitly —
  no change in `tools/call` behaviour.
- **OAuth-client type (Web vs Desktop).** Google's docs recommend
  Desktop for Gmail / Web for Calendar but don't gate execution on
  it. Both endpoints fail with the same Web-app client.
- **Stale grant cache.** Revoked the grant on
  `myaccount.google.com/permissions` and re-granted — same outcome.
- **MCP `initialize` handshake.** Skipping `initialize` from the
  curl tests didn't change the rejection; the relay's `rmcp` client
  does the handshake correctly and gets the same rejection.
- **Token transport bug in relay.** A live test of Anthropic's
  first-party Gmail MCP integration in the same Claude Code session
  succeeded against the same `gmailmcp.googleapis.com/mcp/v1` URL,
  returning real Gmail labels. Confirmed the endpoint accepts
  `tools/call` for *some* clients — just not ours.

## Root cause

**The Google Workspace MCP server family is currently in the
Workspace Developer Preview Program**
(<https://developers.google.com/workspace/preview>). The preview
gate is **project-based**: enrolled projects can invoke `tools/call`;
unenrolled projects get the opaque "caller does not have permission"
response even though `tools/list` is publicly callable as preview
metadata.

Anthropic's, Gemini CLI's, and other first-party projects are
already enrolled, which is why their integrations work end-to-end
against the same endpoint our client receives `403` from.

Enrolment is free, typically **2–5 business days**, and requires
submitting the Google Cloud project ID + project number for
registration.

## Solution

We do not depend on the gateway. Relay forwards the user's bearer
to a **self-hosted MCP server** that calls Google's GA APIs
(`gmail.googleapis.com`, `calendar.googleapis.com`) directly. The
GA APIs are not in preview and accept our existing token (we
verified this with the labels curl above).

### Sidecar choice — `taylorwilsdon/google_workspace_mcp`

| Property | Value |
|---|---|
| Stars / signal | ~2.5k on GitHub, listed #1 on PulseMCP / Skywork / MintMCP independent reviews |
| Maintained | Releases through May 2026, MIT license |
| Language | Python (no npm supply-chain surface) |
| Auth model | Supports `EXTERNAL_OAUTH21_PROVIDER=true` — accepts an externally-issued bearer via `Authorization: Bearer …` per call, no internal OAuth dance needed |
| Coverage | Gmail + Calendar + Drive + Docs + Sheets + Slides + Chat + Forms + Tasks (one container, `MODULES` env filters per instance) |
| Goes through Google's MCP gateway? | No — calls `gmail.googleapis.com` and `calendar.googleapis.com` directly |
| Author credibility | Taylor Wilsdon, Head of Corporate Eng at Yelp; documented security disclosure address |

A third-party security scan (`oathe.ai`) returns a "dangerous"
rating, but the scan's own methodology section confirms it audited
the wrong artifact (a stale Claude Code plugin manifest, not the
Python project) and triggered generic OAuth-handling rules. Not
evidence of malware against the actual repo.

### Architecture

```
Browser ─┬─→ Relay (OAuth start) ─→ accounts.google.com (consent)
         ↓
         Relay (callback, stores bearer in mcp_oauth_credentials)
         ↓
Agent ──→ Relay MCP client ──→ Cloudflare tunnel ──→ Sidecar
                                                       ↓
                                                       gmail.googleapis.com
                                                       calendar.googleapis.com
```

Bearer flow: relay reads from `mcp_oauth_credentials`, attaches
`Authorization: Bearer …` on every MCP request
([src/mcp/client.rs:149](../src/mcp/client.rs)). The sidecar trusts
it (because `EXTERNAL_OAUTH21_PROVIDER=true`) and forwards. No code
change in relay required to switch.

### UX after the swap

End-user experience is identical to the gateway path:

1. Org owner opens **Connections → Gmail** → clicks Connect.
2. Browser hits Google's consent screen, branded "Relay", showing
   only the Gmail scopes.
3. Allow → redirects back → connection card flips to **Connected**.
4. Separate Connect on Calendar repeats the flow with the Calendar
   scopes. (Per-server credential rows, matches the user's mental
   model of granting access to two distinct integrations.)
5. Agent calls Gmail tools transparently — same toolbox, same tool
   names, same audit-log shape. Only latency differs (one extra hop
   through the tunnel, expect +50–200 ms).

### Two sidecar instances vs one

Relay's catalog model is one MCP server URL per integration. Two
ways to honour that with `taylorwilsdon`:

- **Recommended (clean):** Two sidecar containers, each with
  `MODULES=gmail` and `MODULES=calendar` respectively, each on its
  own tunnel hostname. `gmail` and `gcal` catalog rows point at
  their respective hostnames. The agent only sees tools the
  connection actually grants.
- **Cheaper (coarser):** One sidecar exposing both modules, both
  catalog rows point at the same URL. Any agent with the Gmail
  connection also sees Calendar tools and vice versa. Acceptable
  for the demo, not for production scoping.

## Code changes already in place that support this path

All the work done while diagnosing the gateway issue stays useful
under the sidecar architecture — the bearer flow is identical.

| File | What |
|---|---|
| [migrations/00000000000037_mcp_oauth_clients_shared.up.sql](../migrations/00000000000037_mcp_oauth_clients_shared.up.sql) | Nullable `org_id` + partial indexes + RLS split so one platform-owned OAuth client serves every tenant |
| [migrations/00000000000038_mcp_catalog_default_scope.up.sql](../migrations/00000000000038_mcp_catalog_default_scope.up.sql) | Per-catalog OAuth scope set so Google receives `gmail.readonly gmail.compose` (and the Calendar scopes) on every authorize |
| [src/mcp/oauth/shared_seed.rs](../src/mcp/oauth/shared_seed.rs) | Boot-time idempotent upsert of the shared OAuth client, reusing the Login-with-Google credentials |
| [src/mcp/oauth/pg_store.rs](../src/mcp/oauth/pg_store.rs) | `read_shared`, `ClientProvenance::Shared` upsert branch, `canonical_issuer` (drops the trailing-slash Google returns) |
| [src/mcp/oauth/flow.rs](../src/mcp/oauth/flow.rs) | `prompt=consent` + `access_type=offline` for Google issuer — forces a fresh grant + guarantees a refresh token |
| [src/mcp/oauth/refresher.rs](../src/mcp/oauth/refresher.rs) | Refresher consults `read_shared` when no org-scoped row exists |
| [src/mcp/oauth/errors.rs](../src/mcp/oauth/errors.rs) + [src/http/routes/mcp.rs](../src/http/routes/mcp.rs) | Typed `OAuthError::DcrUnsupported` mapped to `409 Conflict` with an actionable CTA |
| [src/mcp/tool.rs](../src/mcp/tool.rs) | `is_error: true` propagation so failed MCP tool calls surface in `tool_calls.is_error` |

The `migrations/00000000000036_mcp_catalog_google.up.sql` catalog
entries (`gmail`, `gcal`) and the per-catalog scope defaults in
migration 38 remain correct — only the `default_transport.url`
needs to flip when the sidecar comes online.

## Remaining work to ship the demo

1. **Stand up sidecar(s).** Run `taylorwilsdon/google_workspace_mcp`
   in two Docker containers (`MODULES=gmail`, `MODULES=calendar`).
   Expose via Cloudflare tunnel (named tunnel so the hostname is
   stable across restarts).
2. **Follow-up migration (`mcp_catalog_gmail_calendar_self_hosted`)** —
   the next available number after the OAuth-vendor-seam PR
   (which has already taken migrations 37–39 for shared clients,
   default scopes, and authorize-extra params).
   `UPDATE mcp_catalog SET default_transport = …` on the `gmail`
   and `gcal` rows to point at the tunnel hostnames.
3. **Sanity-check OAuth discovery** against the tunnel hostname:
   `curl <tunnel>/.well-known/oauth-protected-resource/<path>`
   should return `{"authorization_servers":["https://accounts.google.com/"],…}`.
   If it doesn't, either configure the sidecar to emit RFC 9728
   metadata or pre-seed the `mcp_oauth_clients` row manually via the
   existing `PUT /oauth/client` route.
4. **End-to-end retest.** From the org owner's browser: connect
   Gmail, then Calendar, then dispatch an agent action that uses a
   Gmail tool. Confirm the call lands in the operator's Gmail
   inbox.

## Parallel-track (long-term path)

Enrol the Google Cloud project at
<https://developers.google.com/workspace/preview>. Once approved
(2–5 business days), the existing OAuth client + tokens already
work against `gmailmcp.googleapis.com` and `calendarmcp.googleapis.com`
— no code change. If we want to drop the sidecar later, migration N
flips the catalog URLs back. Don't gate the demo on enrolment
timing.

## Out of scope for this doc

- Microsoft 365 wiring (stub already in
  [shared_seed.rs](../src/mcp/oauth/shared_seed.rs); follow-up PR).
- Google OAuth CASA verification for restricted scopes (only needed
  if we ever leave Testing mode for personal-Gmail demo audiences).
- Tooling to extract a bearer from the encrypted credential store
  for ad-hoc testing (the `examples/dump_bearer.rs` helper used
  during this investigation has been removed; recreate from git
  history if needed).
