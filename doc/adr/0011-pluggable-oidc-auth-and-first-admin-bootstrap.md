# ADR-0011 — Pluggable OIDC auth; Google is one preset; first login bootstraps the admin

- **Status:** Accepted
- **Date:** 2026-06-01
- **Deciders:** core

> **Update (2026-06-08):** the one-release migration window below has closed.
> The Google preset and the `GOOGLE_CLIENT_ID` / `GOOGLE_CLIENT_SECRET` /
> `GOOGLE_REDIRECT_URL` env vars are removed, as are the `/auth/google/*` route
> aliases. The generic OIDC path (`PATOM_OIDC_ISSUER` + `PATOM_OIDC_CLIENT_ID` +
> `PATOM_OIDC_CLIENT_SECRET` + `PATOM_OIDC_REDIRECT_URL`, all required) is now
> the only login path; for Google, set the issuer to `https://accounts.google.com`.

## Context

Authentication today is Google-only and required at startup. The only login
routes are `/auth/google/login` and `/auth/google/callback`
(`src/http/routes/auth.rs`), the only provider impl is
`src/auth/oauth_google.rs`, and `GOOGLE_CLIENT_ID` / `GOOGLE_CLIENT_SECRET` /
`GOOGLE_REDIRECT_URL` are required — the server fails fast without them. This is
correct for our own multi-tenant cloud, where every user has a Google identity.

It blocks the self-hosted edition (see the self-hosting gap plan) on two fronts:

1. **No customer IdP.** A customer running Patom on their own infrastructure
   authenticates against Okta, Entra ID, Keycloak, Ping, or an air-gapped
   internal IdP — not Google. There is no seam to point Patom at a different
   issuer, and Google credentials are mandatory even when unused.

2. **No first admin.** The only way a `users` / `org_members` row comes into
   existence is a successful Google login. On a fresh on-prem database that
   path is unreachable, so there is no way to create the initial administrator.
   The product is unusable on first boot.

These are the same engineering effort: both are "establish an identity without
Google." Solving (1) without (2) yields an instance no one can log into; solving
(2) without (1) has nothing to bootstrap *from*.

Constraints from the existing design we want to preserve:

- The session itself — an HS256 JWT in the `patom_session` cookie, 7-day TTL,
  org-membership lookup, the `Principal` extractor — is provider-independent and
  already correct (ARCHITECTURE §2, ADR-0007). Only *identity establishment* is
  Google-shaped.
- Tenancy is enforced by Postgres RLS keyed off `app.user_id` (ADR-0007). The
  auth change must not touch that boundary; it only decides *which* `user_id`
  the principal carries.
- CLAUDE.md §1 (parse-at-boundary, newtypes), §5 (every external `await`
  bounded by a timeout, every input length-capped), §6 (assert invariants), §12
  (one module error enum).

## Decision

**Identity establishment becomes a provider abstraction. The generic case is
standards-compliant OpenID Connect (authorization-code + PKCE) configured by an
issuer URL; Google becomes one preset of that abstraction. On a database with
zero organizations, the first successful login bootstraps the initial org and
makes that identity its owner.**

### 1. One OIDC seam, presets on top

Add an `OidcProvider` configured from:

- `issuer` — an `IssuerUrl` newtype (https origin, `TryFrom<&str>` at the
  boundary). Endpoints (`authorization_endpoint`, `token_endpoint`, `jwks_uri`)
  are discovered once at startup from
  `{issuer}/.well-known/openid-configuration`, not hardcoded. The discovery
  fetch is wrapped in `tokio::time::timeout` (§5) and its result cached for the
  process lifetime (§9, static-at-startup).
- `client_id`, `client_secret` — newtypes; secret never logged (PII/secret is
  DEBUG-only and stripped, CLAUDE.md §2).
- `redirect_url` — the existing callback contract, unchanged.

Routes become provider-agnostic: `/auth/oidc/login` and `/auth/oidc/callback`.
`/auth/google/*` is kept as a thin alias for one release so existing cloud
deployments and bookmarks don't break, then removed.

`src/auth/oauth_google.rs` is re-expressed as a **preset**: Google is "the OIDC
provider whose issuer is `https://accounts.google.com`, with Google's known
quirks (e.g. `access_type`/`prompt` for refresh tokens) applied." We keep
Google's behavior, we stop special-casing it in the route layer. Adding a
customer IdP is config, not code — consistent with ADR-0008 (every external
dependency is a trait; new impls wire in at the composition root, the turn/route
code does not change).

Per CLAUDE.md §4, we introduce **one** abstraction (OIDC) with **one** preset
(Google) now; we do not pre-build a multi-provider registry or SAML until a
second concrete shape exists.

### 2. Identity key is `(issuer, subject)`, not email

OIDC `sub` is the stable per-issuer identifier; email is mutable and display-only
(captured at DEBUG). A migration adds `oidc_issuer` + `oidc_subject` to the
identity table with a unique constraint on the pair, backfilled for existing
Google rows from their Google `sub`. This prevents two different IdPs that
happen to share an email from colliding, and survives a user's email change.

### 3. First-login admin bootstrap

When a login succeeds **and** the org table is empty, that login creates the
initial organization and inserts the identity as its owner. Guard rails:

- Gated behind an explicit `PATOM_BOOTSTRAP_ADMIN=true` config flag so the
  promotion is a deliberate operator act, never a silent default.
- The "is this the first user?" check asserts the count is exactly zero inside
  the same transaction that performs the insert (§6 — assert the invariant; the
  unique org-bootstrap is a compound update that must not race), so two
  simultaneous first logins cannot both become owner.
- Emitted as a loud `info!(event = "auth.bootstrap.admin", …)` span event.
- This runs under `begin_privileged` (ADR-0007) because at bootstrap there is no
  member yet for RLS to key off — the one documented, audited exception, scoped
  to exactly this path.

Subsequent logins follow the existing path: an identity with no org membership
gets no session (or a pending-invite state), exactly as today.

## Consequences

**What becomes easy:**

- A self-hosted customer points `PATOM_OIDC_ISSUER` at their Keycloak/Okta/Entra
  tenant and logs in — no Google, no code change, air-gap-friendly (discovery
  and JWKS are fetched from the customer's own IdP).
- First boot is self-service: flip `PATOM_BOOTSTRAP_ADMIN`, log in once, you are
  the owner. No seed SQL, no manual `org_members` insert.
- Our own cloud is unaffected: Google is the same behavior, now expressed as a
  preset.

**What becomes hard:**

- The startup config matrix grows (issuer vs. preset, bootstrap flag). Mitigated
  by `values.example.yaml` (release-artifacts task) and by making Google a
  named preset so cloud config stays a one-liner.
- We now depend on IdP discovery + JWKS at startup/first-login. Both are bounded
  by timeouts and cached; a discovery failure fails closed (no login) rather
  than falling back to an insecure path.

**What we live with:**

- The bootstrap path keeps one privileged, RLS-off transaction. It is asserted,
  flag-gated, count-guarded, and logged — the same "documented and used
  sparingly" posture ADR-0007 already takes for schedulers and queue claim.
- Refresh-token quirks remain per-preset (Google needs `access_type=offline`).
  A generic OIDC issuer may surface its own quirks; those land in the preset
  layer, not the route layer — the same place ADR-0008 puts provider specifics.

## Alternatives considered

- **Keep Google, add a separate local username/password store for self-host.**
  Two parallel auth systems to secure, and it ignores that every serious
  customer already has an IdP. Password storage is liability we don't want.
  Rejected.
- **SAML first.** Enterprise-familiar, but heavier (XML, assertions, metadata
  exchange) and most modern IdPs speak OIDC. Build OIDC now; revisit SAML when a
  concrete customer requires it (CLAUDE.md §4 — don't abstract ahead of the
  third occurrence). Rejected for v1.
- **Seed the first admin via a migration / env-var email.** Couples the admin
  identity to config at deploy time and can't verify the person actually
  controls that identity at the IdP. First-login bootstrap proves possession.
  Rejected.
- **A full multi-provider registry (N issuers per instance) now.** Over-built
  for self-host, where one customer IdP is the norm. The OIDC seam leaves room
  to add it later without reshaping the route layer. Deferred.
