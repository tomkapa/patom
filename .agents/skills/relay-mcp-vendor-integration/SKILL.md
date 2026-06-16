---
name: patom-mcp-vendor-integration
description: Diagnose and integrate new MCP vendors (Google, Microsoft 365, Atlassian, GitHub, Slack, Notion, …) in the Patom (patom) project — picks the right seam (catalog data vs OAuth-flow code vs MCP transport) for vendor-specific quirks instead of scattering `is_<vendor>_` predicates across the codebase. Use proactively whenever the user says "add <vendor> as an MCP integration", "Gmail/Calendar/Microsoft OAuth fails", "where does this OAuth fix go", "vendor returns DcrUnsupported", "vendor rejects authorize with Missing required parameter: scope", "vendor returns 403 / caller does not have permission" on tool calls, "refresh tokens dying after an hour", "MCP server breaks for vendor X", or any other moment a vendor-specific OAuth / MCP quirk needs a home in the codebase. Also trigger when planning a new MCP integration even before code is touched, so the diagnosis happens before fixes get written into the wrong layer.
---

# patom-mcp-vendor-integration

When a new MCP vendor breaks (or you're about to add one), this is the playbook for **where the fix belongs** so vendor-specific quirks don't bleed across the codebase. The convention is established in PR #58 (commits `93c8e35` + `2615606`); this skill captures the decision rules.

## Core principle: data tier first

Push every quirk that can be declaratively expressed into the `mcp_catalog` row. Reach for Rust code only when data can't carry the behaviour. Concretely, vendor-specific Rust branches (`if is_google_issuer(...)`, `if vendor == "github"`) are a review-blocking anti-pattern — they're a sign the quirk should have been a catalog column.

## Decision flowchart

A new vendor breaks (or you're about to integrate one). Match the symptom to the seam:

```
Symptom                                            → Seam                         → How
─────────────────────────────────────────────────────────────────────────────────────────
"Missing required parameter: scope" on authorize   → catalog.default_scope        → migration row update
Authorize URL needs non-standard params            → catalog.authorize_extra_params → migration row update
(e.g. Google access_type=offline + prompt=consent,
 Microsoft prompt=consent+prompt=login,
 Atlassian audience=api.atlassian.com)

DcrUnsupported (AS has no registration_endpoint)   → shared OAuth client          → shared_seed.rs spec
(Google, Microsoft 365, any non-DCR vendor)

Refresh tokens die ~1 hour after consent           → catalog.authorize_extra_params → ensure access_type=offline
Cumulative-grant cache returns old scopes          → catalog.authorize_extra_params → ensure prompt=consent

"caller does not have permission" on tools/call    → vendor preview-program gate  → see gmail_mcp_preview_workaround.md;
but tools/list succeeds                              (NOT a code fix)               sidecar or wait for enrolment

Per-call HTTP header required                      → transport layer (FUTURE)     → McpTransportVendor seam, not built yet
(GitHub X-GitHub-Api-Version, Google
 X-Goog-User-Project, Atlassian X-Atlassian-Token)   stop and design before coding

Dynamic per-tenant issuer / authorize URL          → code (FUTURE OAuthVendor)    → not built yet; stop and design
(Shopify {shop}.myshopify.com,
 Microsoft per-tenant /<tenant_id>/)

Token-response post-processing                     → code (FUTURE OAuthVendor)    → not built yet; stop and design
(Salesforce instance_url, Box account_id)

Scope-delimiter different (comma not space)        → code (FUTURE OAuthVendor)    → not built yet; oauth2 crate's
(GitHub user-to-server)                                                             add_scope is hardwired

MCP tool result is_error: true                     → already generic              → no change needed; tool.rs propagates
(Google MCP gateway permission errors)                                              as ToolError::Upstream

Tenant-custom URL with no auth                     → already generic              → McpCatalogStore::ensure_org_scoped
                                                                                    handles it via the /mcp-servers create
```

When the seam is marked "FUTURE", **stop and design** — don't add a `if vendor == X` branch in `flow.rs` or a per-call special-case in `tool.rs`. Surface to the human first; the seam needs to land before the vendor.

## Onboarding checklist for a new vendor

Walk these in order. Most vendors stop after step 1.

1. **Migration: catalog row.** Add an `INSERT INTO mcp_catalog (id, display_name, description, default_transport, auth_kind, ...)` for the new vendor. Same shape as the `gmail` / `gcal` rows in migration 36. If the AS rejects empty scope, also `SET default_scope = '...'` (see migration 38). If the vendor needs non-standard authorize params, also `SET authorize_extra_params = '[...]'::jsonb` (see migration 39). **80% of new vendors stop here.**

2. **Shared OAuth client (no-DCR vendors only).** Verify by curling `<issuer>/.well-known/oauth-authorization-server` — if the response has no `registration_endpoint`, the vendor doesn't support RFC 7591 DCR and needs a platform-owned client:
   - Add `<vendor>_client_id: SecretString` + `<vendor>_client_secret: SecretString` to `AuthSettings` and `RawSettings` in `src/config.rs`. Match the existing `google_*` shape.
   - Uncomment / add the matching branch in `src/mcp/oauth/shared_seed.rs::specs`. Endpoints are stable RFC 8414 values; inline as `const` rather than fetching at boot.
   - Operator provisions credentials out-of-band (e.g. Microsoft Azure App Registration) and sets the env vars before deploy.

3. **Integration test.** Add coverage under `tests/`:
   - For shared-client vendors: extend `tests/pg_mcp_oauth_stores.rs` with a "shared row coexists under issuer X" case.
   - For any vendor: add `tests/auth_mcp_servers.rs` cases that walk `POST /mcp-servers { catalog_id: "<vendor>" } → POST /mcp-servers/{id}/oauth/start → GET /mcp-oauth/callback` end-to-end with a stub AS if needed.
   - Per AGENTS.md §3: failing test first. Confirm it fails for the expected reason before adding the migration.

4. **Front-end card.** Usually nothing — the catalog row drives the `ConnectionsCatalog` page automatically. New `auth_kind` variants need a `ConnectModal` branch (see migration 39 PR adding `noAuth`).

5. **Smoke test.** Run the dev server, click Connect on the new card, finish the consent flow, dispatch one tool call. Verify `tool_calls.is_error` is correctly populated on both success and failure.

## Key file map

| File | Role | When to touch |
|---|---|---|
| `migrations/` | Tier 1 data — catalog rows + columns | New vendor / new quirk that fits as data |
| `src/mcp/catalog.rs` | `McpCatalogEntry` struct + validators (`OAuthAuthorizeExtras`, `default_scope`) | Adding a new declarative quirk column |
| `src/mcp/limits.rs` | Bounded constants for catalog payloads | New collection / string column |
| `src/mcp/oauth/mod.rs::resolve_oauth_client` | **Org → shared precedence (single source of truth)** | Never duplicate this — call it from any new caller |
| `src/mcp/oauth/flow.rs::build_authorize_url` | Takes `extras: &[(&str, &str)]` from catalog; vendor-agnostic | Almost never; only if the OAuth-flow shape itself changes |
| `src/mcp/oauth/shared_seed.rs` | Boot-time shared client upsert | Adding a no-DCR vendor |
| `src/http/routes/mcp.rs::catalog_oauth_config_for_server` | Fetches `default_scope + authorize_extra_params` in one DB hit | Add new fields here if a new catalog column lands |
| `src/mcp/oauth/refresher.rs::refresh_one` | Background token refresh, uses `resolve_oauth_client` | Almost never |
| `src/mcp/tool.rs::McpTool::execute` | Propagates MCP `is_error: true` as `ToolError::Upstream` | Almost never; behaviour is vendor-agnostic |
| `doc/gmail_mcp_preview_workaround.md` | Incident report for Google's preview-program gate | Reference when a vendor's `tools/list` works but `tools/call` 403s |

## Anti-patterns (review-blocking)

These are the patterns the convention exists to prevent. Treat them as flags:

1. **`is_<vendor>_issuer()` predicates.** If you find yourself writing `if is_google_issuer(&client.issuer)` or `if issuer.contains("microsoft")`, the quirk belongs in `mcp_catalog.authorize_extra_params` or a similar column. The whole point of migration 39 was to delete `is_google_issuer`.

2. **Inline `read → read_shared → fail` ladders.** The org → shared precedence lives in exactly one place: `mcp::oauth::resolve_oauth_client`. If you're tempted to write `match store.read(...) { Ok(Some(r)) => r, Ok(None) => store.read_shared(...) ... }` again, stop and use the helper. A missing fallback path silently breaks every shared-client vendor at refresh time.

3. **Issuer string as the vendor discriminator.** Issuer is too brittle — Shopify breaks it (per-shop URL), Microsoft breaks it (per-tenant URL). When code-tier vendor logic eventually lands, the discriminator will be `McpCatalogId`, not issuer.

4. **Vendor-specific scope joining / header forwarding wired into shared code.** GitHub uses comma-separated scopes, others use space. When this arrives, it goes through a future `OAuthVendor` code seam — don't hand-edit the `oauth2` crate calls in `flow.rs`.

5. **Special-casing one vendor in `tool.rs` / `client.rs`.** The MCP protocol-level behaviour (`isError: true`, timeouts, error mapping) is vendor-agnostic. If you think you need a per-vendor branch here, you're probably trying to compensate for a transport-layer quirk that needs the `McpTransportVendor` seam — which doesn't exist yet; stop and design.

6. **Mutating an existing catalog row in a follow-up migration without paired down.** AGENTS.md §14. Catalog rows are append-or-update with explicit migrations; never edit migration 36 to add Gmail back into the seed list.

## What data vs code looks like (worked examples)

| Quirk | Tier | Concrete change |
|---|---|---|
| Google `access_type=offline` + `prompt=consent` | Data | One UPDATE in `mcp_catalog.authorize_extra_params` (migration 39) |
| Gmail/Calendar scope sets | Data | One UPDATE in `mcp_catalog.default_scope` (migration 38) |
| Google has no DCR + needs shared client | Data + boot seeder | `shared_seed.rs::specs` push + `AuthSettings.google_*` fields (already present) |
| Microsoft 365 (hypothetical, next-up) | Data + boot seeder | Catalog row + `shared_seed.rs` Microsoft branch + `AuthSettings.microsoft_*` |
| Atlassian `audience=api.atlassian.com` (hypothetical) | Data | One row in `authorize_extra_params` |
| GitHub comma-separated scopes (hypothetical) | Code (future) | Needs `OAuthVendor` enum + scope-delimiter hook; not built yet |
| Shopify per-shop authorize URL (hypothetical) | Code (future) | Needs `OAuthVendor::resolve_endpoints` from catalog row; not built yet |
| Atlassian `cloudId` in MCP URL (hypothetical) | Code (future) | Needs `McpTransportVendor` per-call URL mutation; not built yet |

## When to escalate to the human

- The vendor's quirk lands in a "FUTURE" row of the flowchart — the code-tier seam doesn't exist yet.
- The fix would require editing `oauth2` crate behaviour (e.g. scope delimiter).
- The vendor requires a credential format that doesn't fit `client_id + client_secret + scope` (e.g. private-key JWT, mTLS).
- The vendor's MCP server is gated behind an enrolment programme (Google Workspace Developer Preview pattern) — point at `doc/gmail_mcp_preview_workaround.md` and ask whether to pursue enrolment or stand up a self-hosted sidecar.

## Reference: where this convention came from

PR #58 collapsed `is_google_issuer` into catalog data, introduced shared OAuth clients (migration 37), pinned per-catalog default scopes (migration 38), promoted authorize-URL extras to catalog data (migration 39), and centralised the org → shared precedence in `mcp::oauth::resolve_oauth_client`. The diagnosis trail for Google specifically lives in `doc/gmail_mcp_preview_workaround.md`. AGENTS.md §1 (newtypes / parse-don't-validate), §3 (TDD), §4 (no clever abstractions), §13 (one logical change per PR), and §14 (paired up/down migrations) are the governing engineering rules.
