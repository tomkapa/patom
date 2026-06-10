//! Per CLAUDE.md §5: every container/bound named here. No magic numbers
//! buried in business logic.

use std::time::Duration;

/// Cookie name carrying the session JWT.
pub const COOKIE_NAME: &str = "patom_session";

/// JWT validity window.
///
/// Hard expiry, no sliding refresh in v1 — user re-logs in once a week.
/// Picked to outlast a long working day in any timezone, but short
/// enough that a leaked token doesn't grant indefinite access.
pub const JWT_TTL: Duration = Duration::from_hours(24 * 7);

/// `oauth_login_states` row TTL. The Google flow happens within seconds;
/// 10 minutes is generous coverage for users who click "Sign in", make
/// coffee, then complete consent.
pub const OAUTH_STATE_TTL: Duration = Duration::from_mins(10);

/// Maximum slug-collision retries when minting the personal org.
///
/// Each retry appends a 4-char random suffix; after 5 attempts we fail
/// loudly — a clash that deep is a sign of a pathological email or a
/// corrupted PRNG.
pub const MAX_SLUG_RETRIES: usize = 5;

/// Hard timeout on the OIDC discovery fetch at startup.
///
/// Covers `{issuer}/.well-known/openid-configuration` plus the JWKS the
/// `openidconnect` client pulls alongside it. Discovery is one-shot at
/// startup; a slow IdP must not hang boot indefinitely. Fail-closed on
/// timeout (no login) per ADR-0011.
pub const OIDC_DISCOVERY_TIMEOUT: Duration = Duration::from_secs(10);

/// Hard timeout on the OIDC token exchange (code → id_token) during the
/// callback. Wraps the single outbound call to the token endpoint.
pub const OAUTH_HTTP_TIMEOUT: Duration = Duration::from_secs(10);

/// Minimum byte length of the JWT signing secret. HS256 best-practice
/// is 256 bits (32 bytes) of random material.
pub const JWT_SECRET_MIN_BYTES: usize = 32;

/// Maximum bytes accepted for an OIDC `name` claim before it is dropped.
///
/// Mirrors the `users.display_name` CHECK (migration 14, ≤200). An
/// over-cap value from a (pluggable) IdP is treated as a best-effort hint
/// and discarded rather than failing the sign-in or the later `users`
/// insert — same posture as `locale`.
pub const MAX_DISPLAY_NAME_BYTES: usize = 200;

// `MAX_AVATAR_URL_BYTES` moved to `crate::types::AVATAR_URL_MAX_BYTES`:
// the auth boundary parses the `picture` claim through `AvatarUrl::try_from`.

/// Cookie name carrying the CSRF double-submit token.
///
/// Non-HttpOnly so the SPA can read it via `document.cookie` and echo
/// the value in an `X-CSRF-Token` header on every mutation. The CSRF
/// middleware compares the two and rejects POSTs that disagree (§10.6).
///
/// Wire-protocol constant — keep in sync with `web/src/lib/api.ts`.
pub const CSRF_COOKIE_NAME: &str = "patom_csrf";

/// HTTP header the SPA echoes the CSRF token in.
///
/// Wire-protocol constant — keep in sync with `web/src/lib/api.ts`.
pub const CSRF_HEADER_NAME: &str = "X-CSRF-Token";

/// Maximum accepted length of either the header or cookie value. The
/// mint produces a 22-char base64url token; anything ≫ that is junk
/// and gets rejected before constant-time comparison.
pub const CSRF_TOKEN_MAX_LEN: usize = 64;

/// Maximum accepted length of an inbound `Origin` / `Referer` header
/// before the Origin/Referer CSRF check treats it as untrusted.
///
/// A real same-origin or SPA-origin value is `scheme://host[:port]` (tens
/// of bytes); a `Referer` adds a path but stays well under this. 2 KiB is
/// generous headroom for a long path while capping the string the check
/// parses through `url::Url` — anything larger is junk and rejected
/// before allocation (CLAUDE.md §5).
pub const ORIGIN_HEADER_MAX_LEN: usize = 2048;

/// Maximum byte length of a per-org rule string (`organizations.default_rule`).
///
/// Bounds the `<organization-rule>` body that the agent worker injects
/// into every system prompt. The cap is mirrored as a SQL `CHECK`
/// constraint in migration 40 — both the smart constructor here and a
/// direct SQL write are rejected over the limit. 16 KiB is generous for
/// a hand-curated CLAUDE.md-style directive while keeping the cached
/// prefix small enough that an admin typo can't bloat every prompt past
/// reason.
pub const MAX_ORG_RULE_BYTES: usize = 16 * 1024;

/// Maximum byte length of a detected-locale primary tag stashed on the
/// `oauth_login_states` row.
///
/// Matches the column's CHECK constraint in migration 20. Real primary
/// tags are 2–8 chars (`vi`, `en`, `zh-Hant` before we strip the
/// region); the cap is generous so a malformed but otherwise harmless
/// `Accept-Language` value is preserved rather than silently dropped —
/// the `Language::from_locale_hint` parser is the final filter.
pub const DETECTED_LOCALE_MAX_LEN: usize = 32;

/// Maximum bytes accepted for an organization display name.
/// Mirrors the migration 14 `organizations.name` CHECK (1..=200).
pub const MAX_ORG_NAME_BYTES: usize = 200;

/// Maximum number of workspaces (orgs) a single user may **own** via
/// self-service creation (`POST /me/orgs`).
///
/// Each created workspace seeds a default agent, so an unbounded create
/// path is a free-resource hole (CLAUDE.md §5). Ten is generous for any
/// realistic person juggling distinct teams/clients while capping a
/// runaway script. Counts owner rows only — being *invited* into many
/// orgs is unbounded and not gated here. Raise deliberately if a real
/// user hits it; deletion (`DELETE /me/org`) frees slots.
pub const MAX_ORGS_PER_USER: i64 = 10;

/// Launch-period override of [`MAX_ORGS_PER_USER`] (#121).
///
/// While `PATOM_LAUNCH_GUARDRAILS` is on, self-service org creation is capped
/// at **one** workspace per identity. Because the free signup credit grant
/// (#154) is keyed `signup:{org_id}` and each identity can now hold exactly
/// one org, this bounds the credit a single Google account can farm to one $2
/// grant — the whole point of the launch guardrail. A launch-only value: when
/// the promo ends, flipping the env switch off restores the generous baseline
/// [`MAX_ORGS_PER_USER`] with no code change.
pub const MAX_ORGS_PER_USER_LAUNCH: i64 = 1;

/// Signup-velocity limit (#121): max OAuth-callback completions admitted from a
/// single trusted client IP within [`SIGNUP_RATE_WINDOW`].
///
/// Tuned to sit far above any legitimate login rate — a real person logs in
/// seconds-to-minutes apart, and even a shared corporate NAT egressing through
/// one IP will not approach 30 interactive Google logins in a minute — while a
/// scripted signup farm doing hundreds per minute trips it immediately. The
/// residual (a NAT with an unusually dense burst) only ever delays a login by
/// the window, never blocks it permanently. Only consulted under
/// `launch_guardrails` with a trusted IP available.
pub const SIGNUP_PER_IP_PER_WINDOW: usize = 30;

/// Rolling window for [`SIGNUP_PER_IP_PER_WINDOW`]. Mirrors the MCP
/// test-connect limiter's one-minute window for operational consistency.
pub const SIGNUP_RATE_WINDOW: Duration = Duration::from_mins(1);

/// Eviction cap on the signup limiter's per-IP bucket map (CLAUDE.md §5).
///
/// A flood of distinct source IPs cannot grow the map unboundedly: at ~80 bytes
/// per bucket this caps the structure at well under 2 MiB. The LRU victim is
/// the least-recently-touched bucket, matching the MCP limiter.
pub const SIGNUP_RATE_BUCKETS_MAX: usize = 16_384;

/// Maximum byte length of `PATOM_COOKIE_DOMAIN` (the shared cookie
/// `Domain` attribute, e.g. `.patom.app`).
///
/// RFC 1035 caps a fully-qualified DNS name at 253 octets; we accept a
/// leading-dot cross-subdomain form (`.patom.app`) so the session and
/// CSRF cookies are visible to both the apex marketing site and the
/// `app.` subdomain. Unset (the default) omits the attribute entirely,
/// preserving the host-only localhost-dev behavior.
pub const COOKIE_DOMAIN_MAX_LEN: usize = 253;

/// Maximum bytes of a single DNS label inside `PATOM_COOKIE_DOMAIN`
/// (the dot-separated segments of e.g. `app.patom.app`).
///
/// RFC 1035 §2.3.4 caps a label at 63 octets. `CookieDomain`'s validator
/// rejects any longer segment at the boundary.
pub const COOKIE_DOMAIN_LABEL_MAX_LEN: usize = 63;

/// Maximum number of CORS allowlist origins accepted in
/// `PATOM_CORS_ALLOWED_ORIGINS`.
///
/// The marketing apex needs exactly one origin (`https://patom.app`);
/// the cap leaves headroom for a staging/preview origin or two while
/// rejecting a runaway comma-separated list that would bloat every
/// preflight's allow-origin matching.
pub const MAX_CORS_ALLOWED_ORIGINS: usize = 8;
