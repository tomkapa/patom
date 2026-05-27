# ADR-0007 — Tenancy via Postgres RLS + per-org envelope encryption

- **Status:** Accepted
- **Date:** 2026-05-22
- **Deciders:** core

## Context

Patom is multi-tenant: one process serves multiple organisations and must never read or write across the tenant boundary. We had two questions to answer:

1. **Where does the tenant check live?** Application code, the database, or both?
2. **How are upstream credentials (OAuth tokens for Notion, Gmail, Slack, …) protected at rest?**

For (1), application-only checks are fragile — a single forgotten `WHERE org_id = ?` is a tenant breach. Database-only checks (Postgres RLS) are robust but require every query to run under the right session principal. Both is overkill if RLS is correct, and under-specified if RLS is the only line of defense.

For (2), plaintext-at-rest is unacceptable for tokens that grant access to a customer's Gmail. Symmetric encryption with a single global key means a single secret unlocks every tenant's data. Per-org keys with envelope encryption let us isolate blast radius.

## Decision

**Every domain row carries `org_id NOT NULL` and is fenced by Postgres Row-Level Security. The app runs as a `NOLOGIN` non-superuser role (`patom_app`) that cannot bypass RLS. MCP credentials are envelope-encrypted at the Rust seam with per-org data keys derived from a master KEK (`PATOM_ORG_KEK`).**

### RLS shape

```sql
ALTER TABLE <tbl> ENABLE ROW LEVEL SECURITY;
ALTER TABLE <tbl> FORCE  ROW LEVEL SECURITY;   -- applies even to owner role
CREATE POLICY <tbl>_org_isolation ON <tbl>
    FOR ALL TO PUBLIC
    USING      (app_user_is_member(org_id))
    WITH CHECK (app_user_is_member(org_id));
```

The predicate keys off `current_setting('app.user_id')::uuid` and `org_members`. Three tenant-context entry points in `src/auth/`:

| Helper                                | Sets                                              | When                                                |
| ------------------------------------- | ------------------------------------------------- | --------------------------------------------------- |
| `begin_as(pool, &Principal)`          | `app.user_id` + `SET LOCAL ROLE patom_app`        | HTTP handlers with a `Principal` extractor          |
| `begin_as_user(pool, user_id)`        | Same; raw `UserId`                                | Worker per-turn writes; tool calls inside a turn    |
| `begin_privileged(pool)`              | `SET LOCAL row_security = off`                    | Schedulers, queue claim, registry refresh (cross-tenant scans) |

`SET LOCAL` scopes each setting to the transaction; a returned connection cannot leak `app.user_id` to the next checkout.

### Hot-path denormalisation

Tables whose RLS predicate would otherwise JOIN through a parent (e.g. `session_messages → sessions`) denormalise `org_id` directly. A `BEFORE INSERT OR UPDATE` parity trigger raises if the row's `org_id` differs from its parent's. Cost: 16 bytes per row. Benefit: one column lookup instead of a JOIN inside the policy.

### Envelope encryption

```text
plaintext   ──── encrypt with per-org DEK ────▶ ciphertext (stored)
DEK         ──── wrap with KEK ──────────────▶ wrapped DEK (stored alongside)
KEK                                            in process memory only,
                                               from PATOM_ORG_KEK env
```

`src/crypto/` exposes `envelope_encrypt(org_id, plaintext)` / `envelope_decrypt`. Per-org DEKs are derived from the master KEK; rotating the KEK is "re-wrap every DEK." Plaintext tokens never land in Postgres.

## Consequences

**What becomes easy:**

- A single forgotten `WHERE org_id = ?` in app code is not a tenant breach — the policy fences it.
- A connection pool checkout-after-no-commit cannot leak the previous transaction's principal (because `SET LOCAL` is transactional).
- A compromised database backup without the KEK leaks no usable upstream credentials.
- Per-tenant data export is a simple `WHERE org_id = ?` query under `begin_privileged`.

**What becomes hard:**

- Tooling that queries Postgres directly (psql sessions, dashboards) must `SET app.user_id` or use the `BYPASSRLS` admin path. Documented and used sparingly.
- Reads inside worker turns are currently privileged (the memory loader, agent-name cache, parent-session history). Writes are fully `_for_user`-scoped, but tightening reads is a follow-up item. Column-level safety (NOT NULL `org_id` + parity triggers) is the safety net.
- Per-table RLS adds policy-evaluation cost. Mitigated by denormalisation and indexed `org_id`.

**What we live with:**

- The OAuth refresh path needs the per-org DEK to decrypt the refresh token; the refresher therefore runs `begin_privileged` to pull the wrapped DEK. The KEK must be present in process memory. Acceptable.
- `RLS row_security = off` for privileged paths is a footgun if widened — the audit rule is "only schedulers, queue claim, and the in-process registry refresh."

## Alternatives considered

- **App-level checks only.** A single missed predicate is a breach. Rejected.
- **Schema-per-tenant.** Operational nightmare (N migrations per release), and cross-tenant operator queries become expensive. Rejected.
- **Global symmetric key for token encryption.** One secret unlocks every tenant. Rejected.
- **External KMS (AWS KMS, GCP KMS) for KEK.** Reasonable future addition, but not required for v1 — KEK in env-from-vault is sufficient when the deployment is self-hosted. Layered on later without schema impact.
