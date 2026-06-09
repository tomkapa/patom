//! OIDC login / callback (ADR-0011). Public — the auth layer is not
//! applied here (the whole point is to mint a session for an as-yet
//! unauthenticated visitor). Provider-agnostic: Google is one issuer
//! behind the `OidcAuth` seam.
//!
//! Flow:
//!   1. `GET  /auth/oidc/login?return_to=…` — mint random `state` +
//!      PKCE verifier + nonce, insert into `oauth_login_states`, redirect
//!      to the IdP.
//!   2. `GET  /auth/oidc/callback?code=…&state=…` — consume the stored
//!      verifier + nonce, exchange the code for a verified id_token,
//!      upsert the user, resolve the org they land in (see
//!      [`resolve_active_org`] — self-service on cloud; bootstrap /
//!      join-by-invite / deny on self-host), mint a JWT, set the session
//!      cookie, redirect to the validated `return_to` (or `/`).
//!
//! The routes are provider-neutral (`/auth/oidc/*`). Operators register
//! `{PATOM_OIDC_REDIRECT_URL}` = `…/auth/oidc/callback` with their IdP.

use axum::Router;
use axum::extract::{Query, State};
use axum::http::HeaderMap;
use axum::http::header::ACCEPT_LANGUAGE;
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::get;
use axum_extra::extract::cookie::{Cookie, CookieJar, SameSite};
use chrono::{DateTime, Utc};
use cookie::time::Duration as CookieDuration;
use serde::Deserialize;
use tracing::info;

use crate::auth::{
    CookieDomain, Language, LocaleHint, OAuthStateRow, OrgId, UserId, limits::COOKIE_NAME,
};

use super::super::csrf::{build_csrf_cookie, mint_csrf_token};
use super::super::error::HttpError;
use super::super::state::AppState;

pub(super) fn router() -> Router<AppState> {
    Router::new()
        .route("/auth/oidc/login", get(login))
        .route("/auth/oidc/callback", get(callback))
}

#[derive(Debug, Deserialize)]
struct LoginQuery {
    #[serde(default)]
    return_to: Option<String>,
}

#[derive(Debug, Deserialize)]
struct CallbackQuery {
    code: String,
    state: String,
    /// Google may return `error=access_denied` when the user clicks
    /// "cancel". Surface as a 400 so the FE can react.
    #[serde(default)]
    error: Option<String>,
}

async fn login(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<LoginQuery>,
) -> Result<Redirect, HttpError> {
    let start = state.oauth.start()?;
    let now = now_utc(&state);
    let expires = now
        + chrono::Duration::from_std(crate::auth::limits::OAUTH_STATE_TTL)
            .unwrap_or_else(|_| chrono::Duration::seconds(600));
    let safe_return = query.return_to.and_then(|r| sanitize_return_to(&r));
    // Extract the primary tag from the inbound Accept-Language so the
    // callback has a locale fallback when Google's userinfo doesn't
    // carry one. Walks all entries for the first that maps to a
    // supported `Language`; the bound is enforced by the `LocaleHint`
    // newtype which matches the `oauth_login_states.detected_locale`
    // column CHECK.
    let detected_locale = headers
        .get(ACCEPT_LANGUAGE)
        .and_then(|v| v.to_str().ok())
        .and_then(extract_primary_locale);
    state
        .users
        .insert_oauth_state(&OAuthStateRow {
            state: start.state.clone(),
            pkce_verifier: start.pkce_verifier.clone(),
            nonce: start.nonce.clone(),
            redirect_to: safe_return,
            detected_locale,
            created_at: now,
            expires_at: expires,
        })
        .await?;
    Ok(Redirect::to(start.authorize_url.as_str()))
}

/// Walk an `Accept-Language` header for the first entry whose primary
/// subtag maps to a supported [`Language`], returning that subtag.
///
/// `Accept-Language` follows the shape `tag(;q=...)?(,tag(;q=...)?)*` —
/// e.g. `fr-CA,vi;q=0.9,en;q=0.8`. We honour entry order over `q=`
/// (browsers always emit entries in user-preference order, with `q=`
/// merely ranking duplicates) and skip entries whose primary tag we
/// don't speak so a header like `fr-CA,vi;q=0.9` resolves to `vi`
/// rather than stashing `fr` and falling back to English. Returns
/// `None` when no supported tag is present; the OAuth callback then
/// hands `None` to [`Language::from_locale_hint`] which falls through
/// to `Language::DEFAULT`.
fn extract_primary_locale(raw: &str) -> Option<LocaleHint> {
    raw.split(',')
        .filter_map(|entry| primary_subtag(entry.trim()))
        .find(|tag| Language::parse(tag).is_some())
        .and_then(|tag| LocaleHint::try_from(tag.as_str()).ok())
}

/// Lowercase the primary subtag of one `Accept-Language` entry. Returns
/// `None` for an empty entry or one whose primary part is blank.
fn primary_subtag(entry: &str) -> Option<String> {
    let primary = entry.split(['-', '_', ';']).next()?.trim();
    if primary.is_empty() {
        return None;
    }
    Some(primary.to_ascii_lowercase())
}

async fn callback(
    State(state): State<AppState>,
    Query(query): Query<CallbackQuery>,
    jar: CookieJar,
) -> Result<Response, HttpError> {
    if let Some(err) = query.error.as_deref() {
        return Err(HttpError::BadRequest(format!("oauth: {err}")));
    }
    let now = now_utc(&state);
    let state_token =
        crate::auth::OAuthState::try_from(query.state.as_str()).map_err(HttpError::Parse)?;
    let consumed = state.users.consume_oauth_state(&state_token, now).await?;

    let profile = state
        .oauth
        .exchange(&query.code, &consumed.pkce_verifier, &consumed.nonce)
        .await?;
    let upserted = state.users.upsert_from_oidc(&profile, now).await?;

    let active_org = resolve_active_org(
        &state,
        &upserted.user,
        profile.locale.as_ref(),
        consumed.detected_locale.as_ref(),
        now,
    )
    .await?;

    let jar = remint_session(&state, jar, upserted.user.id, active_org)?;
    info!(
        event = "auth.login.success",
        patom.user.id = %upserted.user.id,
        // `None` → org-less onboarding session (cloud first login). The
        // empty-string fallback keeps the field low-cardinality + present.
        patom.org.id = active_org.map(|o| o.to_string()).unwrap_or_default(),
        patom.user.new = upserted.is_new_user,
    );
    let dest = consumed.redirect_to.unwrap_or_else(|| "/".to_owned());
    // axum-extra: tupling a CookieJar with a Redirect produces a
    // response that carries both `Set-Cookie` and `Location` headers.
    Ok((jar, Redirect::to(&dest)).into_response())
}

/// Pick (or establish) the org a freshly-authenticated user lands in.
/// `Ok(None)` is an **org-less** landing — a cloud first-login with no
/// pending invite, who must create a workspace through onboarding before
/// they have an active org.
///
/// - Already a member → their first org.
/// - Cloud build (`state.cloud`) with no invite → `None` (no auto-create).
/// - Self-host (`PATOM_BOOTSTRAP_ADMIN` set, invite-only): the
///   genuinely-first login (empty org table) bootstraps the initial org
///   as owner; a later login joins whatever org it was invited to;
///   anyone else is denied — no per-user personal org.
async fn resolve_active_org(
    state: &AppState,
    user: &crate::auth::User,
    profile_locale: Option<&LocaleHint>,
    detected_locale: Option<&LocaleHint>,
    now: DateTime<Utc>,
) -> Result<Option<crate::auth::OrgId>, HttpError> {
    let orgs = state.users.list_user_orgs(user.id).await?;
    if let Some(first) = orgs.first() {
        return Ok(Some(first.org_id));
    }
    let display = user
        .display_name
        .clone()
        .unwrap_or_else(|| user.email.as_str().to_owned());
    let slug_seed = user
        .email
        .as_str()
        .split('@')
        .next()
        .expect("invariant: str::split always yields at least one element");
    // Per-org language seed. The id_token `locale` claim wins when
    // present; the inbound Accept-Language primary tag stashed at
    // /auth/oidc/login is the fallback. `Language::from_locale_hint` is
    // the single normalization point, falling back to `Language::DEFAULT`.
    let language = Language::from_locale_hint(
        profile_locale.map(LocaleHint::as_str),
        detected_locale.map(LocaleHint::as_str),
    );

    if state.bootstrap_admin {
        // Self-host / invite-only (ADR-0011). First login on an empty DB
        // becomes the admin; the count-guarded path emits the audit event.
        if let Some(new_org) = state
            .users
            .bootstrap_initial_org_as_owner(user.id, slug_seed, &display, language, now)
            .await?
        {
            seed_recruiter(state, new_org.id, language).await?;
            return Ok(Some(new_org.id));
        }
        // Otherwise the admin must have invited this email; consume the
        // invite and join that existing org (which already has its
        // agents — no seeding).
        if let Some(org_id) = state
            .orgs
            .join_pending_invites(user.id, &user.email, now)
            .await?
        {
            return Ok(Some(org_id));
        }
        return Err(HttpError::Forbidden(
            "no organization — ask your workspace admin for an invite",
        ));
    }

    // An email-matched pending invite wins so an invited newcomer who
    // arrives via the sign-in redirect lands in the inviting org instead
    // of a throwaway personal one (the token-accept endpoint covers
    // already-logged-in users). Applies to both build modes; only reached
    // when the user has no orgs yet, so there is no active org to
    // surprise-switch.
    if let Some(org_id) = state
        .orgs
        .join_pending_invites(user.id, &user.email, now)
        .await?
    {
        return Ok(Some(org_id));
    }
    // `None` also covers the race where a concurrent callback for the same
    // user claimed the invite between its SELECT and UPDATE (see pg_store
    // `join_pending_invites`) — that winner already inserted the
    // membership. Re-check before deciding to org-less / auto-create.
    let orgs = state.users.list_user_orgs(user.id).await?;
    if let Some(first) = orgs.first() {
        return Ok(Some(first.org_id));
    }
    // Cloud build: no auto-create. The user lands org-less and creates
    // their workspace through onboarding (`POST /me/orgs`). Self-host
    // (non-cloud, non-bootstrap) keeps today's behavior: a per-user
    // personal org seeded with a default agent.
    if state.cloud {
        return Ok(None);
    }
    let new_org = state
        .users
        // `None` cap: the auto-created personal org is never capped.
        .create_personal_org(user.id, slug_seed, &display, language, None, now)
        .await?;
    seed_recruiter(state, new_org.id, language).await?;
    Ok(Some(new_org.id))
}

/// Seed a freshly-minted org's default agent in `language` so the
/// session cookie we mint resolves to a usable workspace. Idempotent —
/// re-running for an existing org returns the existing recruiter's id.
/// `recruiter_seed` only fails on a registry-body invariant
/// violation, which is a server bug → 500, not a 4xx.
///
/// `pub(super)` so the self-service create handler (`me::create_org`)
/// seeds the new workspace's recruiter through the same path the
/// OAuth callback uses — one copy of the seeding policy.
pub(super) async fn seed_recruiter(
    state: &AppState,
    org_id: crate::auth::OrgId,
    language: Language,
) -> Result<(), HttpError> {
    let seed = crate::app::recruiter_seed(&state.prompts, language).map_err(|e| {
        tracing::error!(
            event = "auth.callback.recruiter_seed_build_failed",
            error = ?e,
        );
        HttpError::Internal
    })?;
    crate::app::seed_recruiter_for_org(&state.agents, org_id, seed).await?;
    Ok(())
}

/// Mint a fresh session JWT for `(user, org)` and return the jar with the
/// session + CSRF cookies set. `org = None` mints an org-less onboarding
/// session. The single home for the cookie choreography shared by the
/// OAuth callback, `switch_org`, `accept_invite`, `create_org`, and
/// `delete_org` — change session-cookie semantics here, once.
pub(super) fn remint_session(
    state: &AppState,
    jar: CookieJar,
    user: UserId,
    org: Option<OrgId>,
) -> Result<CookieJar, HttpError> {
    let token = state.jwt.mint(user, org)?;
    let session_cookie = build_session_cookie(
        token,
        state.cookie_secure(),
        state.jwt.ttl_secs(),
        state.cookie_domain(),
    );
    let csrf_cookie = build_csrf_cookie(
        mint_csrf_token(),
        state.cookie_secure(),
        state.jwt.ttl_secs(),
        state.cookie_domain(),
    );
    Ok(jar.add(session_cookie).add(csrf_cookie))
}

pub(super) fn build_session_cookie(
    token: String,
    secure: bool,
    ttl_secs: i64,
    domain: Option<&CookieDomain>,
) -> Cookie<'static> {
    let mut cookie = Cookie::new(COOKIE_NAME, token);
    cookie.set_http_only(true);
    cookie.set_path("/");
    cookie.set_same_site(SameSite::Lax);
    cookie.set_secure(secure);
    cookie.set_max_age(CookieDuration::seconds(ttl_secs));
    if let Some(d) = domain {
        cookie.set_domain(d.as_str().to_owned());
    }
    cookie
}

fn now_utc(state: &AppState) -> DateTime<Utc> {
    state.clock.now_utc()
}

/// Only allow relative-path return URLs (no scheme, no host) so the
/// callback can't be turned into an open-redirect to attacker domains.
pub(super) fn sanitize_return_to(raw: &str) -> Option<String> {
    if raw.starts_with('/') && !raw.starts_with("//") && raw.len() <= 2048 {
        Some(raw.to_owned())
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_cookie_sets_domain_when_configured() {
        let domain = CookieDomain::try_from(".patom.app").expect("valid");
        let cookie = build_session_cookie("jwt".to_string(), true, 3600, Some(&domain));
        // `Cookie::domain()` normalizes away the RFC-6265 leading dot.
        assert_eq!(cookie.domain(), Some("patom.app"));
        // HttpOnly invariant must survive the Domain addition.
        assert_eq!(cookie.http_only(), Some(true));
    }

    #[test]
    fn session_cookie_omits_domain_when_unset() {
        let cookie = build_session_cookie("jwt".to_string(), false, 3600, None);
        assert!(cookie.domain().is_none());
        assert_eq!(cookie.secure(), Some(false));
    }

    #[test]
    fn sanitize_return_to_rejects_absolute_and_protocol_relative() {
        assert!(sanitize_return_to("/agents").is_some());
        assert!(sanitize_return_to("https://evil.test").is_none());
        assert!(sanitize_return_to("//evil.test").is_none());
    }
}
