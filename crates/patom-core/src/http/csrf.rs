//! CSRF protection via double-submit cookie + header.
//!
//! Flow:
//! 1. After authentication (OAuth callback, `/me`, `/auth/switch-org`)
//!    the response sets a non-HttpOnly `patom_csrf` cookie carrying a
//!    fresh random token.
//! 2. The SPA reads the cookie via `document.cookie` and echoes the
//!    value in the `X-CSRF-Token` header on every state-changing
//!    request (anything that is not GET / HEAD / OPTIONS).
//! 3. The [`require_csrf`] middleware compares the cookie value to the
//!    header value (constant-time) and rejects mismatches with 403.
//!
//! The middleware sits inside the authenticated subtree (applied AFTER
//! [`require_principal`]), so unauthenticated routes like the OAuth
//! login + callback are never reached by this layer — they have no
//! cookie to compare yet.
//!
//! ## Second layer: Origin/Referer
//!
//! `SameSite=Lax` blocks cross-*site* GETs but not same-eTLD+1 POSTs, so
//! once Patom is internet-facing the double-submit token is the only real
//! CSRF defense. [`require_trusted_origin`] adds a standard second layer:
//! on every state-changing request it validates the `Origin` header (or,
//! absent that, `Referer`) against the origins Patom already knows —
//! this server's own origin (`oauth_redirect_base`), the configured SPA
//! origin (`web_base_url`), and the CORS allowlist (`cors_allowed_origins`).
//!
//! It is *lenient on absent headers*: a request carrying neither `Origin`
//! nor `Referer` passes. A browser always attaches `Origin` to a
//! cross-origin unsafe request and the page cannot suppress it, so the
//! actual CSRF vector is still rejected; the double-submit token covers
//! the rest. Non-browser clients that omit both are not the CSRF threat
//! model. A `null` origin (sandboxed iframe) is present-and-untrusted and
//! is rejected.

use axum::extract::{Request, State};
use axum::http::{Method, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum_extra::extract::cookie::{Cookie, CookieJar, SameSite};
use cookie::time::Duration as CookieDuration;
use oauth2::CsrfToken;
use thiserror::Error;
use tracing::error;
use url::Url;

use super::state::AppState;
use crate::auth::CookieDomain;
use crate::auth::limits::{
    CSRF_COOKIE_NAME, CSRF_HEADER_NAME, CSRF_TOKEN_MAX_LEN, ORIGIN_HEADER_MAX_LEN,
};

/// Errors raised by the CSRF middleware.
///
/// Both variants map to 403 at the HTTP boundary. Distinct names so
/// telemetry can tell apart "client forgot to echo" from "client
/// echoed a stale value".
#[derive(Debug, Error)]
pub(super) enum CsrfError {
    #[error("csrf token missing from request")]
    Missing,
    #[error("csrf token mismatch")]
    Mismatch,
    #[error("request origin not allowed")]
    UntrustedOrigin,
}

impl IntoResponse for CsrfError {
    fn into_response(self) -> Response {
        error!(event = "http.csrf.rejected", error = ?self);
        (StatusCode::FORBIDDEN, self.to_string()).into_response()
    }
}

/// Mint a fresh CSRF token. Re-uses `oauth2::CsrfToken::new_random` so
/// we route every random secret in the codebase through the same vetted
/// primitive (already used at `auth/oauth_google.rs:99` for the OAuth
/// state nonce). Output is 22 chars of base64url alphabet.
pub(super) fn mint_csrf_token() -> String {
    CsrfToken::new_random().secret().clone()
}

/// Build the `patom_csrf` cookie. `secure` mirrors the same flag used
/// for the session cookie; `ttl_secs` keeps the CSRF cookie aligned
/// with the session JWT so they expire together. NOT HttpOnly — the
/// SPA must read this value via `document.cookie`.
pub(super) fn build_csrf_cookie(
    token: String,
    secure: bool,
    ttl_secs: i64,
    domain: Option<&CookieDomain>,
) -> Cookie<'static> {
    csrf_cookie_with(token, secure, ttl_secs, domain)
}

/// Build an expired CSRF cookie. Pairs with logout — the session cookie
/// expires the same way, see `me::logout`. The `domain` MUST match the
/// live cookie's `Domain` or the browser keys the expiry cookie
/// separately and never clears the session.
pub(super) fn build_expired_csrf_cookie(
    secure: bool,
    domain: Option<&CookieDomain>,
) -> Cookie<'static> {
    csrf_cookie_with(String::new(), secure, 0, domain)
}

fn csrf_cookie_with(
    value: String,
    secure: bool,
    ttl_secs: i64,
    domain: Option<&CookieDomain>,
) -> Cookie<'static> {
    let mut cookie = Cookie::new(CSRF_COOKIE_NAME, value);
    cookie.set_http_only(false);
    cookie.set_path("/");
    cookie.set_same_site(SameSite::Lax);
    cookie.set_secure(secure);
    cookie.set_max_age(CookieDuration::seconds(ttl_secs));
    if let Some(d) = domain {
        cookie.set_domain(d.as_str().to_owned());
    }
    cookie
}

/// Tower middleware enforcing the double-submit invariant on every
/// state-changing request. Safe methods (GET, HEAD, OPTIONS) pass
/// through unconditionally — they are idempotent and not exploitable
/// via CSRF in any standard attacker model.
pub(super) async fn require_csrf(
    jar: CookieJar,
    request: Request,
    next: Next,
) -> Result<Response, CsrfError> {
    let method = request.method();
    if matches!(method, &Method::GET | &Method::HEAD | &Method::OPTIONS) {
        return Ok(next.run(request).await);
    }

    let cookie_value = jar.get(CSRF_COOKIE_NAME).ok_or(CsrfError::Missing)?;
    let header_value = request
        .headers()
        .get(CSRF_HEADER_NAME)
        .and_then(|h| h.to_str().ok())
        .ok_or(CsrfError::Missing)?;

    let cookie_bytes = cookie_value.value().as_bytes();
    let header_bytes = header_value.as_bytes();

    // Reject pathological lengths before constant-time eq so an
    // attacker can't lengthen the comparison window. Both values come
    // from our own mint, so anything > the cap is junk.
    if cookie_bytes.is_empty()
        || header_bytes.is_empty()
        || cookie_bytes.len() > CSRF_TOKEN_MAX_LEN
        || header_bytes.len() > CSRF_TOKEN_MAX_LEN
    {
        return Err(CsrfError::Mismatch);
    }

    if !constant_time_eq(cookie_bytes, header_bytes) {
        return Err(CsrfError::Mismatch);
    }

    Ok(next.run(request).await)
}

/// Constant-time equality on two byte slices. Returns `false` for
/// different-length inputs (length is not secret here).
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff: u8 = 0;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Tower middleware rejecting state-changing requests that name an
/// untrusted `Origin` (or, absent that, `Referer`). Runs alongside
/// [`require_csrf`] inside the authenticated subtree. Safe methods pass
/// through; so does a request carrying neither header (see the module
/// docs for why lenient-on-absent is sound).
pub(super) async fn require_trusted_origin(
    State(state): State<AppState>,
    request: Request,
    next: Next,
) -> Result<Response, CsrfError> {
    let method = request.method();
    if matches!(method, &Method::GET | &Method::HEAD | &Method::OPTIONS) {
        return Ok(next.run(request).await);
    }

    // Prefer `Origin`; fall back to `Referer`. Borrow the header just long
    // enough to reach a verdict — no owned copy, and the borrow ends
    // before `request` moves into `next.run`. The length cap lives here,
    // the one boundary the value crosses (CLAUDE.md §5).
    let headers = request.headers();
    let trusted = match headers
        .get(header::ORIGIN)
        .or_else(|| headers.get(header::REFERER))
        .and_then(|h| h.to_str().ok())
    {
        // Neither header present, or an absurdly long value a real
        // browser never sends: allow. The double-submit token still
        // guards this request.
        None => true,
        Some(raw) if raw.len() > ORIGIN_HEADER_MAX_LEN => true,
        // Present but unparseable / opaque ("null") → not a tuple origin
        // → untrusted.
        Some(raw) => header_origin(raw).is_some_and(|candidate| {
            is_trusted_origin(
                &state.oauth_redirect_base,
                state.web_base_url.as_deref(),
                &state.cors_allowed_origins,
                &candidate,
            )
        }),
    };

    if !trusted {
        return Err(CsrfError::UntrustedOrigin);
    }

    Ok(next.run(request).await)
}

/// Reduce a header value to its canonical origin (`scheme://host[:port]`).
/// `Origin` is already an origin; `Referer` is a full URL whose origin we
/// extract. Returns `None` for a malformed or opaque (`"null"`) value.
///
/// Not `config::parse_origin`: that is config-boundary validation (scheme
/// allowlist, rejects any path) — it would reject a valid `Referer`, which
/// always carries one. Here `.origin()` strips the path for us, and the
/// caller bounds the length before calling.
fn header_origin(value: &str) -> Option<String> {
    let origin = Url::parse(value).ok()?.origin();
    origin.is_tuple().then(|| origin.ascii_serialization())
}

/// Whether `candidate` (an already-normalized origin from
/// [`header_origin`]) is one Patom trusts for a state-changing request:
/// this server's own origin (`own_redirect_base`), the configured SPA
/// origin (`spa_origin`), or a CORS-allowlisted origin (`cors_origins`).
/// The config values are normalized to the same ASCII origin form at the
/// config boundary (`config::parse_origin`).
fn is_trusted_origin(
    own_redirect_base: &str,
    spa_origin: Option<&str>,
    cors_origins: &[String],
    candidate: &str,
) -> bool {
    // `oauth_redirect_base` carries a path in prod, so reduce it to its
    // origin before comparing.
    if header_origin(own_redirect_base).as_deref() == Some(candidate) {
        return true;
    }
    if spa_origin == Some(candidate) {
        return true;
    }
    cors_origins.iter().any(|o| o == candidate)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constant_time_eq_matches_eq_for_equal_inputs() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(constant_time_eq(b"", b""));
    }

    #[test]
    fn constant_time_eq_rejects_different_inputs() {
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"abcd"));
        assert!(!constant_time_eq(b"abc", b""));
    }

    #[test]
    fn mint_csrf_token_produces_nonempty_url_safe_value() {
        let t = mint_csrf_token();
        assert!(!t.is_empty());
        assert!(
            t.len() <= CSRF_TOKEN_MAX_LEN,
            "mint exceeded {CSRF_TOKEN_MAX_LEN} chars: {t}"
        );
        // base64url alphabet: A-Z a-z 0-9 _ -
        assert!(
            t.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        );
    }

    #[test]
    fn csrf_cookie_sets_domain_when_configured() {
        let domain = CookieDomain::try_from(".patom.app").expect("valid");
        let cookie = build_csrf_cookie("tok".to_string(), true, 3600, Some(&domain));
        // `Cookie::domain()` normalizes away the RFC-6265 leading dot.
        assert_eq!(cookie.domain(), Some("patom.app"));
        // Unchanged invariants travel with the Domain.
        assert_eq!(cookie.http_only(), Some(false));
        assert_eq!(cookie.same_site(), Some(SameSite::Lax));
    }

    #[test]
    fn csrf_cookie_omits_domain_when_unset() {
        let cookie = build_csrf_cookie("tok".to_string(), true, 3600, None);
        assert!(cookie.domain().is_none());
    }

    #[test]
    fn expired_csrf_cookie_carries_domain_and_zero_max_age() {
        let domain = CookieDomain::try_from(".patom.app").expect("valid");
        let cookie = build_expired_csrf_cookie(true, Some(&domain));
        // Both must hold or logout fails to clear the shared cookie.
        assert_eq!(cookie.domain(), Some("patom.app"));
        assert_eq!(cookie.max_age(), Some(CookieDuration::seconds(0)));
    }

    #[test]
    fn header_origin_passes_through_an_origin_value() {
        // An `Origin` header is already `scheme://host[:port]`.
        assert_eq!(
            header_origin("https://patom.app").as_deref(),
            Some("https://patom.app")
        );
        assert_eq!(
            header_origin("http://localhost:5173").as_deref(),
            Some("http://localhost:5173")
        );
    }

    #[test]
    fn header_origin_extracts_origin_from_a_referer_url() {
        // A `Referer` is a full URL — only its origin survives.
        assert_eq!(
            header_origin("https://patom.app/threads/42?tab=x#frag").as_deref(),
            Some("https://patom.app")
        );
    }

    #[test]
    fn header_origin_rejects_opaque_and_malformed_values() {
        // Sandboxed iframes send a literal `null` origin — opaque, not a
        // tuple origin, so untrusted.
        assert_eq!(header_origin("null"), None);
        assert_eq!(header_origin("not a url"), None);
        assert_eq!(header_origin(""), None);
    }

    #[test]
    fn is_trusted_origin_accepts_own_spa_and_cors_origins() {
        let cors = vec!["https://patom.app".to_string()];
        // Own origin — `oauth_redirect_base` carries a path; the check
        // reduces it to its origin first.
        assert!(is_trusted_origin(
            "https://api.patom.app/mcp-oauth/callback",
            Some("https://app.patom.app"),
            &cors,
            "https://api.patom.app",
        ));
        // Configured SPA origin.
        assert!(is_trusted_origin(
            "https://api.patom.app/mcp-oauth/callback",
            Some("https://app.patom.app"),
            &cors,
            "https://app.patom.app",
        ));
        // CORS-allowlisted origin.
        assert!(is_trusted_origin(
            "https://api.patom.app/mcp-oauth/callback",
            Some("https://app.patom.app"),
            &cors,
            "https://patom.app",
        ));
    }

    #[test]
    fn is_trusted_origin_rejects_an_unrelated_origin() {
        assert!(!is_trusted_origin(
            "https://api.patom.app/mcp-oauth/callback",
            Some("https://app.patom.app"),
            &["https://patom.app".to_string()],
            "https://evil.example",
        ));
        // No SPA configured (same-origin deployment) and empty CORS —
        // only the own origin is trusted.
        assert!(!is_trusted_origin(
            "http://localhost:8080",
            None,
            &[],
            "http://localhost:5173",
        ));
    }
}
