//! Per CLAUDE.md §5: every container/bound named here. No magic numbers
//! buried in business logic.

use std::time::Duration;

/// Cookie name carrying the session JWT.
pub const COOKIE_NAME: &str = "relay_session";

/// JWT validity window.
///
/// Hard expiry, no sliding refresh in v1 — user re-logs in once a week.
/// Picked to outlast a long working day in any timezone, but short
/// enough that a leaked token doesn't grant indefinite access.
pub const JWT_TTL: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// `oauth_login_states` row TTL. The Google flow happens within seconds;
/// 10 minutes is generous coverage for users who click "Sign in", make
/// coffee, then complete consent.
pub const OAUTH_STATE_TTL: Duration = Duration::from_secs(10 * 60);

/// Maximum slug-collision retries when minting the personal org.
///
/// Each retry appends a 4-char random suffix; after 5 attempts we fail
/// loudly — a clash that deep is a sign of a pathological email or a
/// corrupted PRNG.
pub const MAX_SLUG_RETRIES: usize = 5;

/// Maximum bytes accepted from Google's userinfo response. The spec
/// payload is well under 4 KiB; an oversize response is a sign of a
/// hijacked provider and is rejected at the boundary.
pub const MAX_USERINFO_BYTES: usize = 8 * 1024;

/// Hard timeout on every outbound call to Google during the OAuth
/// exchange. Wraps both the token exchange and the userinfo fetch.
pub const OAUTH_HTTP_TIMEOUT: Duration = Duration::from_secs(10);

/// Minimum byte length of the JWT signing secret. HS256 best-practice
/// is 256 bits (32 bytes) of random material.
pub const JWT_SECRET_MIN_BYTES: usize = 32;

/// Cookie name carrying the CSRF double-submit token.
///
/// Non-HttpOnly so the SPA can read it via `document.cookie` and echo
/// the value in an `X-CSRF-Token` header on every mutation. The CSRF
/// middleware compares the two and rejects POSTs that disagree (§10.6).
///
/// Wire-protocol constant — keep in sync with `web/src/lib/api.ts`.
pub const CSRF_COOKIE_NAME: &str = "relay_csrf";

/// HTTP header the SPA echoes the CSRF token in.
///
/// Wire-protocol constant — keep in sync with `web/src/lib/api.ts`.
pub const CSRF_HEADER_NAME: &str = "X-CSRF-Token";

/// Maximum accepted length of either the header or cookie value. The
/// mint produces a 22-char base64url token; anything ≫ that is junk
/// and gets rejected before constant-time comparison.
pub const CSRF_TOKEN_MAX_LEN: usize = 64;

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
