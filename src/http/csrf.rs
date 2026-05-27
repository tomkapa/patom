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

use axum::extract::Request;
use axum::http::{Method, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum_extra::extract::cookie::{Cookie, CookieJar, SameSite};
use cookie::time::Duration as CookieDuration;
use oauth2::CsrfToken;
use thiserror::Error;
use tracing::error;

use crate::auth::limits::{CSRF_COOKIE_NAME, CSRF_HEADER_NAME, CSRF_TOKEN_MAX_LEN};

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
pub(super) fn build_csrf_cookie(token: String, secure: bool, ttl_secs: i64) -> Cookie<'static> {
    csrf_cookie_with(token, secure, ttl_secs)
}

/// Build an expired CSRF cookie. Pairs with logout — the session cookie
/// expires the same way, see `me::logout`.
pub(super) fn build_expired_csrf_cookie(secure: bool) -> Cookie<'static> {
    csrf_cookie_with(String::new(), secure, 0)
}

fn csrf_cookie_with(value: String, secure: bool, ttl_secs: i64) -> Cookie<'static> {
    let mut cookie = Cookie::new(CSRF_COOKIE_NAME, value);
    cookie.set_http_only(false);
    cookie.set_path("/");
    cookie.set_same_site(SameSite::Lax);
    cookie.set_secure(secure);
    cookie.set_max_age(CookieDuration::seconds(ttl_secs));
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
}
