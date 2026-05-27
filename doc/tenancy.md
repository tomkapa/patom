# Tenancy — orgs, users, Google OAuth, Postgres RLS

How patom-rs answers "whose row is this?" at every layer. Read top to
bottom on first contact; later edits should keep the prose terse and
mechanism-first. The shipped code in `src/auth/`,
`migrations/00000000000014–00000000000019_*.sql`, and the
`tests/auth_*.rs` integration probes is the source of truth — this
document explains *why* the shape is what it is so changes don't drift
from the design.

## 1. Mental model

Two identity tables and one bridge. **Users** are global (an email
identifies a human across the system). **Organizations** are tenants —
the unit of data isolation. **`org_members(org_id, user_id, role)`** is
many-to-many: one user can belong to many orgs, one org has many users.
Every domain row (`agents`, `mcp_servers`, `sessions`,
`session_messages`, `prompt_requests`, every memory table,
`scheduled_tasks`, etc.) carries a `org_id NOT NULL` column. Postgres
Row Level Security policies — keyed on the request's `app.user_id`
session GUC and `app_user_is_member(org_id)` — keep one tenant's queries
from seeing another's rows.

Sessions are single-owner: `sessions.created_by_user_id` is the human
who started the DAG. Worker turns inherit that identity so writes done
on the agent's behalf run under the *human's* principal, not an
unscoped service role. Schedulers (reflection, librarian,
scheduled-tasks) read the task/checkpoint's stored
`(org_id, created_by_user_id)` and use the same mechanism when the
fire-tx enqueues a new prompt request.

## 2. Schema

Migrations 14–19 build the surface in one trunk. Each migration ships
together with paired up/down SQL and the Rust-side code that reads it
(per CLAUDE.md §14).

```text
migrations/
  00000000000014_tenancy_foundation.up.sql
    └─ users, user_identities, organizations, org_members,
       oauth_login_states; app_current_user_id() + app_user_is_member()
       helpers; patom_app NOLOGIN role; mcp_servers retrofit
       (org_id NOT NULL, UNIQUE(org_id, alias), FORCE RLS, policy).
  00000000000015_agents_tenancy.up.sql
    └─ agents.org_id, per-org default + name uniqueness, RLS.
  00000000000016_sessions_tenancy.up.sql
    └─ sessions.{org_id, created_by_user_id} + DAG-pair index re-keyed
       to lead with org_id; session_messages.org_id denormalised with
       parent-org trigger; RLS on both.
  00000000000017_memory_tenancy.up.sql
    └─ org_id on memory_events, agent_memories, contradiction_events,
       reflection_checkpoints, validation_events; shared
       enforce_memory_row_parent_org() parity trigger; RLS.
  00000000000018_runtime_tenancy.up.sql
    └─ org_id on prompt_requests, prompt_request_dags, session_leases,
       session_turn_seq, prompt_response_chunks,
       prompt_response_streams; idempotency_key per-org;
       enforce_runtime_row_parent_{session,request}_org triggers; RLS.
  00000000000019_scheduling_tenancy.up.sql
    └─ scheduled_tasks.{org_id, created_by_user_id};
       enforce_scheduled_tasks_parent_org trigger; RLS.
```

### Identity tables

```text
users               (id, email CITEXT UNIQUE, display_name, avatar_url, ts)
user_identities     (provider, subject) PK; FK → users.id; provider IN
                    ('google') today, widens to other OAuth providers
                    later without schema work.
organizations       (id, name, slug CITEXT UNIQUE, ts)
org_members         (org_id, user_id) PK; role ∈ owner|admin|member
oauth_login_states  short-lived row keyed by random `state`; carries
                    PKCE verifier + optional `return_to`; expires after
                    OAUTH_STATE_TTL (10 min).
```

No RLS on identity tables in v1 — `/me` returns the caller's user row
and their memberships; identity is single-tenant per row already
(`user_id` and `org_members` lookups filter at the SQL level).

### Domain-table pattern

Every retrofit follows the same shape:

```sql
ALTER TABLE <tbl> ADD COLUMN org_id UUID NOT NULL
    REFERENCES organizations(id) ON DELETE CASCADE;

ALTER TABLE <tbl> ENABLE ROW LEVEL SECURITY;
ALTER TABLE <tbl> FORCE ROW LEVEL SECURITY;  -- applies even to owner

CREATE POLICY <tbl>_org_isolation ON <tbl>
    FOR ALL TO PUBLIC
    USING      (app_user_is_member(org_id))
    WITH CHECK (app_user_is_member(org_id));
```

`FORCE` is non-negotiable: without it the table owner role (`patom` in
dev/tests, which is a superuser) bypasses every policy. Even with
FORCE, Postgres superusers bypass RLS — so the app runs queries as
`patom_app` (created in migration 14, `NOLOGIN`, no superuser bit).
The `auth::begin_as_user(pool, user_id)` helper does
`SET LOCAL ROLE patom_app` after setting `app.user_id`. Privileged
infrastructure paths (queue claim, scheduler scans) use
`auth::begin_privileged` which `SET LOCAL row_security = off` (only
works because the connection role *is* the owner — production should
move to a dedicated BYPASSRLS role, see §10).

### Denormalised `org_id` + parity triggers

Hot-path tables denormalise `org_id` rather than JOIN-through-parent
in the policy: a policy executes per-row on every query, and a JOIN in
the predicate compounds cost. Denormalisation costs 16 bytes per row
and is kept honest by a `BEFORE INSERT OR UPDATE OF org_id, <parent_fk>`
trigger that raises if `org_id` differs from the parent's. Two shared
trigger functions cover most tables: `enforce_memory_row_parent_org`
(memory_* tables → agents.org_id),
`enforce_runtime_row_parent_session_org` /
`enforce_runtime_row_parent_request_org` (runtime tables → sessions or
prompt_requests org_id). `session_messages`, `agents`, and
`scheduled_tasks` get their own one-off trigger functions because their
parent relationships are slightly different.

## 3. Auth subsystem (`src/auth/`)

```text
src/auth/
  mod.rs           re-exports + begin_as / begin_as_user / begin_privileged
  types.rs         UserId, OrgId (uuid_newtype!); Email, OrgSlug;
                   Role (str_enum! owner/admin/member); Principal;
                   GoogleProfile; User; OrgMembership; UpsertedUser.
  error.rs         AuthError (Unauthenticated, NotMember,
                   OAuthStateInvalid, OAuthProvider, EmailUnverified,
                   Jwt, Parse, Db, Internal).
  jwt.rs           JwtSigner (HS256, jsonwebtoken v9). JwtClaims =
                   { sub: UserId, org: OrgId, iat, exp }. Time via
                   SharedClock (CLAUDE.md §11). Verify checks signature
                   + exp with 5s leeway.
  oauth_google.rs  GoogleOAuth wrapping oauth2 v5 (PKCE + state).
                   Uses oauth2::reqwest::Client (the crate's own
                   reqwest re-export — the project's reqwest 0.13
                   doesn't impl AsyncHttpClient).
  store.rs         UserStore trait (upsert_from_google,
                   create_personal_org, list_user_orgs, membership,
                   read_user, insert_oauth_state, consume_oauth_state).
  pg_store.rs      PgUserStore. Identity tables aren't RLS-protected,
                   so all methods run with SET LOCAL row_security = off
                   inside their own tx.
  limits.rs        COOKIE_NAME, JWT_TTL (7d), OAUTH_STATE_TTL (10min),
                   MAX_SLUG_RETRIES (5), MAX_USERINFO_BYTES (8 KiB),
                   OAUTH_HTTP_TIMEOUT (10s), JWT_SECRET_MIN_BYTES (32).
```

### Three tenant-context entry points

| Helper                                | Sets                                              | When to use                                                    |
| ------------------------------------- | ------------------------------------------------- | -------------------------------------------------------------- |
| `begin_as(pool, &Principal)`          | `app.user_id = principal.user_id` + role          | HTTP handlers that have a `Principal` extractor                |
| `begin_as_user(pool, user_id)`        | `app.user_id = user_id` + role                    | Worker per-turn writes, tool calls inside a worker turn        |
| `begin_privileged(pool)`              | `SET LOCAL row_security = off`                    | Infra: queue claim, scheduler scans, LISTEN, registry refresh  |

`SET LOCAL` / `set_config(.., true)` scope each setting to the
transaction. A connection returned to the pool without commit/rollback
cannot leak `app.user_id` to the next checkout (the §5/§10 footgun
task1.md:80 calls out).

## 4. Sign-in flow (Google OAuth Authorization Code + PKCE)

```text
       Browser                    patom-rs                     Google
          │                          │                            │
          │ GET /auth/google/login   │                            │
          │ ?return_to=/dashboard    │                            │
          │────────────────────────▶│                            │
          │                          │ insert oauth_login_states  │
          │                          │ (state, PKCE verifier,     │
          │                          │  return_to, expires_at)    │
          │                          │                            │
          │ 302 to Google's auth url │                            │
          │ ◀────────────────────────│                            │
          │                          │                            │
          │ GET .../o/oauth2/v2/auth?client_id&scope=openid+email │
          │ +profile&state&code_challenge=...                     │
          │ ─────────────────────────────────────────────────────▶│
          │                          │                            │
          │ user consents            │                            │
          │ ◀──────────────────────────────────────────────────────│
          │                          │                            │
          │ 302 to /auth/google/callback?code&state                │
          │                          │                            │
          │ GET /auth/google/callback│                            │
          │────────────────────────▶│                            │
          │                          │ DELETE oauth_login_states  │
          │                          │ WHERE state=$1 AND         │
          │                          │       expires_at > now()   │
          │                          │ → verifier, return_to      │
          │                          │                            │
          │                          │ POST .../oauth2/token      │
          │                          │ + PKCE verifier ──────────▶│
          │                          │ ◀────── access_token       │
          │                          │ GET .../v1/userinfo ──────▶│
          │                          │ ◀────── {sub,email,...}    │
          │                          │                            │
          │                          │ upsert users + identity    │
          │                          │ → (user, is_new_user)      │
          │                          │ if no memberships:         │
          │                          │   create_personal_org      │
          │                          │   + seed_default_agent     │
          │                          │ mint JWT(sub=user, org=…)  │
          │                          │                            │
          │ 302 to <return_to>       │                            │
          │ Set-Cookie: patom_session=<jwt>;                       │
          │   HttpOnly; SameSite=Lax; Secure?                      │
          │ ◀────────────────────────│                            │
```

`return_to` is sanitised to a relative path (`starts_with('/')`,
`!starts_with("//")`, length ≤ 2048) so the callback can't become an
open-redirect. The OAuth `state` row is single-use (deleted at consume)
and time-limited (10 min) — replays and stale callbacks both raise
`AuthError::OAuthStateInvalid` → 401.

## 5. HTTP surface

```text
src/http/routes/
  auth.rs          GET /auth/google/login, GET /auth/google/callback
                   (public — outside the principal layer)
  healthz.rs       GET /healthz (public)
  me.rs            GET /me, POST /auth/logout, POST /auth/switch-org
                   (private — Principal extractor)
  agents.rs        all CRUD — private
  mcp.rs           all CRUD — private
  threads.rs       /threads, /threads/{id}/messages, SSE — private
  memory.rs        per-agent memory ops — private
  prompts.rs       POST /prompts, GET /requests/:id/stream,
                   POST /requests/:id/cancel — private
```

`src/http/routes/mod.rs` splits the router:

```rust
let public  = Router::new()
    .merge(auth::router())     // /auth/google/{login,callback}
    .merge(healthz::router()); // /healthz

let private = Router::new()
    .merge(prompts::router())
    .merge(agents::router())
    .merge(mcp::router())
    .merge(memory::router())
    .merge(threads::router())
    .merge(me::router())
    .route_layer(middleware::from_fn_with_state(
        state.clone(), require_principal));

Router::new().merge(public).merge(private).with_state(state)
    .layer(TraceLayer::new_for_http())
```

`route_layer` is required (not `.layer`) so the middleware wraps only
the private subtree. Reversing this gates `/auth/google/*` behind a
cookie, deadlocking sign-in.

### `require_principal` middleware

Reads `patom_session` cookie via `axum_extra::extract::CookieJar`,
verifies the JWT, looks up the user's membership of the `org` claim
(via `UserStore::membership`), inserts a `Principal` into the request's
extensions, calls `next`. Failures collapse to
`AuthError::Unauthenticated` (401); membership lookups that return
`None` map to `AuthError::NotMember(org)` (403). The middleware logs a
`warn!` line distinguishing signature failures from expiry so operators
can debug, but the wire response is intentionally opaque.

### `Principal` axum extractor

A handler that takes `principal: Principal` reads the value the
middleware stashed in request extensions. Missing → 500
(`MissingPrincipal`) because that means a handler tagged with
`Principal` was reached without the middleware in front of it — a
routing-graph bug. Handler signature pattern:

```rust
async fn list_mcp_servers(
    State(state): State<AppState>,
    principal: Principal,
) -> Result<Json<Vec<McpServerResponse>>, HttpError> {
    let mut tx = auth::begin_as(&state.pool, &principal).await?;
    let rows = sqlx::query_as::<_, _>("SELECT … FROM mcp_servers ORDER BY alias")
        .fetch_all(&mut *tx).await?;
    tx.commit().await?;
    Ok(Json(rows.into_iter().map(Into::into).collect()))
}
```

### Cookie attributes

`HttpOnly; Path=/; SameSite=Lax; Max-Age=604800` and `Secure` when
`PATOM_COOKIE_SECURE=true`. `SameSite=Lax` blocks cross-site GETs
(except top-level navigations — the OAuth callback works) but **does
not** block same-site POSTs from a malicious page on the same eTLD+1.
See §10 follow-ups about CSRF on `POST /auth/switch-org` and
`POST /auth/logout`.

## 6. Worker turn-tx

`PgPromptQueue::claim_next_session` joins `sessions` to return
`ClaimedSession { session_id, prompts, lease, org_id, created_by_user_id }`.
The claim itself runs `begin_privileged` (cross-tenant scan). After
claim, each worker-side mutation calls a `_for_user(user_id, …)` store
variant that opens `auth::begin_as_user(pool, claim.created_by_user_id)`
internally — so the write runs under `app.user_id = original human` +
`patom_app` role. RLS fires. A worker can only write into the session
it was claim-bound to.

The design decision worth remembering: we considered threading a single
`&mut Transaction` through the turn (one tx per claimed turn). The
`async-trait` + `Box<dyn Tool>::call` lifetimes made this a swamp.
Per-method `_for_user` (one short tx per store call) achieves the same
RLS-enforcement outcome at the cost of more `BEGIN`/`COMMIT`
roundtrips. Acceptable.

15 `_for_user` variants across `SessionStore`, `ResponseSink`,
`DagBudget`, `PromptQueue`, `MemoryStore`, `AgentStore`,
`ScheduledTaskStore`. `ToolCallContext` carries `acting_user_id` so
tools (memory_*, send_message, create_agent, schedule_task, etc.) pass
it into their store calls.

Reads stay privileged (memory section loader, agent name cache, parent
session history) — the cascade of widening `_for_user` to every read
path would touch caches and the agent-prompt assembly. Column-level
safety (NOT NULL `org_id` + parity triggers) fences cross-tenant data
at insert time; the agent only acts on the session it claimed.
Tightening reads is a follow-up.

## 7. Schedulers

Three schedulers exist (`reflection_scheduler`, `librarian_scheduler`,
`scheduled_task_scheduler`). They scan cross-tenant tables to find
work — that scan runs `begin_privileged`. When they enqueue a new
`prompt_requests` row, the enqueue carries the right `org_id` and
`created_by_user_id`:

- **Reflection:** reads `(org_id, created_by_user_id)` off the
  conversation session and propagates into the reflection
  `NewPromptRequest`.
- **Librarian:** the original brief picked the first org member as a
  synthetic user; with the agents retrofit, the librarian now reads
  through `agents → org_members LIMIT 1` and uses that user as the
  acting principal for resolution turns.
- **Scheduled tasks:** the `scheduled_tasks` row stores its own
  `(org_id, created_by_user_id)` (set when the agent called
  `schedule_task`). Fire-tx reads them directly.

The previously-needed `tenancy_for_agent` JOIN helper in
`src/scheduling/scheduler.rs` is gone — the row itself is the source
of truth.

## 8. Test scaffolding

```text
tests/common/
  pg.rs            TestDb::fresh seeds default_user_id, default_org_id,
                   default_agent_id (the agent is seeded into
                   default_org_id post-retrofit).
  auth.rs          test_jwt, test_oauth, user_store, seed_principal
                   (mints a fresh user+org+membership and returns a
                   cookie), principal_for_default_org (mints a cookie
                   for the already-seeded default).
  harness.rs       WorkerHarness with default_user_id/default_org_id
                   exposed.
  embedding.rs     FakeEmbeddingProvider (unchanged).

tests/auth_*.rs    e2e isolation probes (one per subsystem):
  auth_mcp_servers.rs         (3 tests)
  auth_agents.rs              (3)
  auth_threads.rs             (3)
  auth_memory.rs              (3)
  auth_prompts.rs             (3)
  auth_scheduled_tasks.rs     (3)
  auth_worker_writes.rs       (2)
```

Each probe sets covers the same three shapes: (1) 401 when unauth, (2)
empty list / no-op for a fresh principal, (3) cross-org isolation —
seed two orgs, write a row in each via the privileged path, confirm
each principal sees only their own. `auth_worker_writes.rs` is the
keystone: it proves a worker-side `_for_user` insert is RLS-rejected
when the acting principal is from a different org than the session's.

## 9. Where things live (quick map)

- **Schema:** `migrations/00000000000014_..._19_*.sql`
- **Auth subsystem:** `src/auth/`
- **HTTP wiring:** `src/http/auth_layer.rs` (middleware + Principal
  extractor), `src/http/state.rs` (AppState fields: `jwt`, `oauth`,
  `users`, `clock`, `cookie_secure`)
- **OAuth routes:** `src/http/routes/auth.rs`
- **`/me` + switch-org + logout:** `src/http/routes/me.rs`
- **Public liveness:** `src/http/routes/healthz.rs`
- **Tenant-context helpers:** `src/auth/mod.rs::{begin_as, begin_as_user,
  begin_privileged}`
- **Composition root:** `src/app.rs` (constructs JwtSigner, GoogleOAuth,
  PgUserStore; OAuth callback calls `seed_default_agent_for_org` on
  first sign-up)
- **Env config:** `src/config.rs::AuthSettings`; documented in
  `.env.example`
- **Test helpers:** `tests/common/auth.rs`, `tests/common/pg.rs`
- **E2E probes:** `tests/auth_*.rs`

## 10. Frontend sign-in flow (to be implemented)

Backend contract is settled. The SPA in `web/` needs:

### 10.1 Request layer

Every API client must send credentials. With cookie-based auth, set
fetch's `credentials: "include"` on every call. If the SPA uses
`@tanstack/react-query`, configure a default fetch wrapper.

Recommended shape:

```ts
async function api<T>(path: string, init?: RequestInit): Promise<T> {
  const res = await fetch(path, {
    ...init,
    credentials: "include",
    headers: { "content-type": "application/json", ...(init?.headers ?? {}) },
  });
  if (res.status === 401) {
    // Redirect to sign-in, preserving the current URL as return_to.
    const back = encodeURIComponent(window.location.pathname + window.location.search);
    window.location.href = `/auth/google/login?return_to=${back}`;
    throw new AuthRedirect();
  }
  if (!res.ok) throw new ApiError(res.status, await res.text());
  return res.json();
}
```

### 10.2 Routing / route guards

- A `<Protected>` wrapper around every authed route. On mount, fetch
  `/me`. While loading: render a spinner. On 401: redirect to
  `/auth/google/login?return_to=<current>`. On 200: stash the response
  in a global store (zustand) and render children.
- An unprotected `/sign-in` page (optional) with a single button that
  navigates to `/auth/google/login`.

### 10.3 `/me` consumer

```ts
type Me = {
  user: { id: string; email: string; display_name: string | null; avatar_url: string | null };
  orgs: Array<{ id: string; name: string; slug: string; role: "owner" | "admin" | "member" }>;
  active_org_id: string;
  role: "owner" | "admin" | "member";
};
```

Cache for the session, invalidate on logout / switch-org.

### 10.4 Org switcher

A dropdown in the app shell that lists `me.orgs`. Selecting one:

```ts
await api("/auth/switch-org", {
  method: "POST",
  body: JSON.stringify({ org_id: selected.id }),
});
// Backend re-mints the JWT and re-sets the cookie. Invalidate /me
// + every tenant-scoped query and refetch.
```

### 10.5 Logout

```ts
await fetch("/auth/logout", { method: "POST", credentials: "include" });
// Cookie is expired by the response. Clear the local /me cache and
// redirect to /sign-in (or to /auth/google/login).
```

### 10.6 CSRF posture (defer or address)

The session cookie is `SameSite=Lax`. State-changing endpoints
(`POST /auth/switch-org`, `POST /auth/logout`, `POST /prompts`, every
`POST /mcp-servers`, etc.) are protected against cross-site GETs by
SameSite, but a malicious page on the same eTLD+1 can still POST. Two
options for v1:

- **Double-submit token:** on first `/me`, return a CSRF token in a
  non-HttpOnly cookie; SPA echoes it in an `X-CSRF-Token` header;
  middleware compares.
- **Origin/Referer check:** middleware rejects POSTs whose `Origin`
  header is unset or doesn't match `PATOM_PUBLIC_ORIGIN`. Simpler, no
  client change.

Pick before opening the SPA to public deploy. Origin-check is the
v1-shaped default.

### 10.7 Error mapping

| Status | Reason                              | UI behaviour                                                  |
| ------ | ----------------------------------- | ------------------------------------------------------------- |
| 401    | Cookie missing/expired/invalid      | Redirect to `/auth/google/login` with `return_to`             |
| 403    | Member-of-another-org or role denied | Banner: "no access to this resource in <org name>"           |
| 502    | OAuth provider unavailable          | Banner on /sign-in: "Google is unreachable, try again"        |

## 11. Production checklist (deferred items, ordered)

These are not implemented; they are not blocking for the foundation
but should be revisited before public deploy.

1. **Operator docs in README + a real-Google smoke test.** This file
   covers the *why*; README should have the operator "how" (where to
   register a Google client, how to generate `PATOM_JWT_SECRET`, what
   to set for `GOOGLE_REDIRECT_URL` in dev vs prod).
2. **CSRF for POST endpoints** (§10.6 above).
3. **Integration tests for `/me`, `/auth/logout`, `/auth/switch-org`.**
   Implemented but no `tests/auth_me.rs` exists. Mirror the
   `auth_*.rs` shape: hit each endpoint with and without cookie, with
   an org id the principal isn't a member of, etc.
4. **Tighten worker reads to `_for_user`.** Writes are RLS-enforced;
   reads still privileged. The cascade through the memory loader +
   agent name cache + parent_history is the work.
5. **Server-side JWT revocation.** v1 = rotate `PATOM_JWT_SECRET` to
   invalidate everyone. v2 = a `revoked_jwts(jti, expires_at)` table
   checked in `require_principal`.
6. **Sliding-window session refresh.** v1 = hard 7-day TTL → user
   re-logs in. v2 = touch a `last_seen_at` on `/me` and re-mint the
   cookie when within 24h of expiry.
7. **GitHub / other OAuth providers.** `user_identities.provider`
   CHECK widens when they land. The store API
   (`upsert_from_google` → `upsert_from_oauth(provider, profile)`) is
   the seam to genericise.
8. **Dedicated `patom_scheduler` BYPASSRLS role for production.** v1
   runs the scheduler as the table owner with `row_security` toggled
   per tx. Production with role separation should add a `BYPASSRLS`
   role for the scheduler-only paths.
9. **Per-org rate limits / quotas** on `mcp_servers`, `agents`,
   `scheduled_tasks`. Today the global caps in `*/limits.rs` apply
   per-org by construction (each org gets `MAX_AGENTS` etc. because
   the `count(*)` in `create` is implicitly per-org via RLS) — but
   nothing prevents a single tenant from exhausting the global
   connection pool / queue depth. v2 = explicit per-org caps with a
   429 response.
