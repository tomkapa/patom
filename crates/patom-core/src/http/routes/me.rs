//! Authenticated identity endpoints. Mounted under the auth layer so
//! every handler here receives a [`Principal`].

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, patch, post};
use axum::{Json, Router};
use axum_extra::extract::cookie::{Cookie, CookieJar, SameSite};
use cookie::time::Duration as CookieDuration;
use serde::{Deserialize, Serialize};

use crate::auth::{
    AuthError, InviteToken, Language, OrgId, OrgMembership, OrganizationRule, Principal, Role,
    User,
    limits::{COOKIE_NAME, CSRF_COOKIE_NAME, CSRF_TOKEN_MAX_LEN},
};

use super::super::csrf::{build_csrf_cookie, build_expired_csrf_cookie, mint_csrf_token};
use super::super::error::HttpError;
use super::super::state::AppState;
use super::auth::build_session_cookie;

pub(super) fn router() -> Router<AppState> {
    Router::new()
        .route("/me", get(me))
        .route("/auth/logout", post(logout))
        .route("/auth/switch-org", post(switch_org))
        .route("/me/invites/accept", post(accept_invite))
        .route("/me/org/language", patch(set_org_language))
        .route("/me/org/rule", patch(set_org_rule))
}

#[derive(Debug, Serialize)]
struct MeResponse {
    user: UserView,
    orgs: Vec<OrgView>,
    active_org_id: OrgId,
    role: Role,
}

#[derive(Debug, Serialize)]
struct UserView {
    id: crate::auth::UserId,
    email: String,
    display_name: Option<String>,
    avatar_url: Option<String>,
}

#[derive(Debug, Serialize)]
struct OrgView {
    id: OrgId,
    name: String,
    slug: String,
    role: Role,
    /// Per-org language driving both the agent's `<language>` directive
    /// and the web app's i18n. The FE switches its locale on every
    /// change of this value for the active org.
    default_language: Language,
    /// Per-org rule body injected into every agent's system prompt as
    /// `<organization-rule>...</organization-rule>`. `None` when the
    /// org hasn't configured a rule; the FE editor seeds with this
    /// value on load.
    default_rule: Option<String>,
    /// `null` → FE renders the default tile in the OrgSwitcher.
    avatar_url: Option<String>,
    /// `false` → the user has not walked the /onboarding wizard for this
    /// org yet, and the FE gate (`OnboardingGate`) routes them there.
    /// Flipped to `true` by the wizard's final step via
    /// `PATCH /me/org { onboarded: true }`.
    onboarded: bool,
}

async fn me(
    State(state): State<AppState>,
    principal: Principal,
    jar: CookieJar,
) -> Result<Response, HttpError> {
    let user = state
        .users
        .read_user(principal.user_id)
        .await?
        .ok_or(AuthError::Unauthenticated)?;
    let orgs = state.users.list_user_orgs(principal.user_id).await?;
    // Only mint a CSRF cookie if the client doesn't already have a
    // valid one — /me is polled, and rotating the token every poll
    // defeats the SPA's cached value and bloats every response with a
    // Set-Cookie. Mirror require_csrf's accept-band so a malformed
    // cookie gets repaired instead of locking the session out of
    // every POST.
    let needs_csrf = jar.get(CSRF_COOKIE_NAME).is_none_or(|c| {
        let bytes = c.value().as_bytes();
        bytes.is_empty() || bytes.len() > CSRF_TOKEN_MAX_LEN
    });
    let response_jar = if needs_csrf {
        jar.add(build_csrf_cookie(
            mint_csrf_token(),
            state.cookie_secure(),
            state.jwt.ttl_secs(),
            state.cookie_domain(),
        ))
    } else {
        jar
    };
    let body = Json(MeResponse {
        user: view_user(user),
        orgs: orgs.iter().map(view_org).collect(),
        active_org_id: principal.active_org_id,
        role: principal.role,
    });
    Ok((response_jar, body).into_response())
}

fn view_user(u: User) -> UserView {
    UserView {
        id: u.id,
        email: u.email.as_str().to_owned(),
        display_name: u.display_name,
        avatar_url: u.avatar_url.map(|a| a.as_str().to_owned()),
    }
}

fn view_org(m: &OrgMembership) -> OrgView {
    OrgView {
        id: m.org_id,
        name: m.org_name.clone(),
        slug: m.org_slug.as_str().to_owned(),
        role: m.role,
        default_language: m.default_language,
        default_rule: m.default_rule.as_ref().map(|r| r.as_str().to_owned()),
        avatar_url: m.avatar_url.as_ref().map(|a| a.as_str().to_owned()),
        onboarded: m.onboarded,
    }
}

async fn logout(State(state): State<AppState>, jar: CookieJar) -> Response {
    let mut cookie = Cookie::new(COOKIE_NAME, "");
    cookie.set_path("/");
    cookie.set_http_only(true);
    cookie.set_same_site(SameSite::Lax);
    cookie.set_secure(state.cookie_secure());
    cookie.set_max_age(CookieDuration::seconds(0));
    // The expiry cookie MUST carry the same Domain as the live session
    // cookie, or the browser treats them as distinct cookies and the
    // session survives logout.
    if let Some(d) = state.cookie_domain() {
        cookie.set_domain(d.as_str().to_owned());
    }
    let jar = jar.add(cookie).add(build_expired_csrf_cookie(
        state.cookie_secure(),
        state.cookie_domain(),
    ));
    (StatusCode::NO_CONTENT, jar).into_response()
}

#[derive(Debug, Deserialize)]
struct SwitchOrgRequest {
    org_id: OrgId,
}

async fn switch_org(
    State(state): State<AppState>,
    principal: Principal,
    jar: CookieJar,
    Json(req): Json<SwitchOrgRequest>,
) -> Result<Response, HttpError> {
    let role = state
        .users
        .membership(principal.user_id, req.org_id)
        .await?
        .ok_or(AuthError::NotMember(req.org_id))?;
    let token = state.jwt.mint(principal.user_id, req.org_id)?;
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
    let jar = jar.add(session_cookie).add(csrf_cookie);
    Ok((
        jar,
        Json(serde_json::json!({ "active_org_id": req.org_id, "role": role })),
    )
        .into_response())
}

#[derive(Debug, Deserialize)]
struct AcceptInviteRequest {
    /// Cleartext URL token from the invite link (`/i/{slug}/{token}`).
    token: String,
}

/// `POST /me/invites/accept` — redeem an invite by its URL token. The
/// caller is already authenticated (logged-out clicks bounce through
/// `/sign-in` first), so this joins the inviting org on their behalf and
/// re-mints the session with that org active — mirroring `switch_org` —
/// so the invitee lands in the inviting workspace. Possession of the
/// token is the authorisation (§1: parse it at the boundary).
async fn accept_invite(
    State(state): State<AppState>,
    principal: Principal,
    jar: CookieJar,
    Json(req): Json<AcceptInviteRequest>,
) -> Result<Response, HttpError> {
    let token = InviteToken::try_from(req.token.as_str()).map_err(HttpError::Parse)?;
    let now = state.clock.now_utc();
    let accepted = state
        .orgs
        .accept_invite(principal.user_id, &token, now)
        .await?;
    let session = state.jwt.mint(principal.user_id, accepted.org_id)?;
    let session_cookie = build_session_cookie(
        session,
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
    let jar = jar.add(session_cookie).add(csrf_cookie);
    Ok((
        jar,
        Json(serde_json::json!({
            "active_org_id": accepted.org_id,
            "role": accepted.role,
        })),
    )
        .into_response())
}

#[derive(Debug, Deserialize)]
struct SetOrgLanguageRequest {
    /// `"en"` or `"vi"`. `Language` is a `str_enum!` so deserialize
    /// rejects anything outside the supported set at the boundary
    /// (CLAUDE.md §1: parse, don't validate).
    language: Language,
}

/// `PATCH /me/org/language` — switch the active org's `default_language`.
///
/// Authorization: owner or admin only. Members get 403 — language is an
/// org-wide setting, not a per-user preference. The backend is the
/// authority; the FE only hides the switcher as a UX nicety.
///
/// Side effect: invalidates the in-process [`OrgLanguageResolver`] cache
/// so the agent worker picks up the new value on the next turn rather
/// than waiting for natural TTL expiry. Cache invalidation is whole-keyset
/// today (see `PgOrgLanguageResolver::invalidate_all`); cheap because the
/// cache is small and language changes are rare.
async fn set_org_language(
    State(state): State<AppState>,
    principal: Principal,
    Json(req): Json<SetOrgLanguageRequest>,
) -> Result<Response, HttpError> {
    // `Principal.role` is whatever the JWT was minted with — stale if
    // the caller was demoted after the cookie was issued. Re-read the
    // current membership and authorize from that, so a recent demotion
    // takes effect without waiting for the session TTL.
    let role = state
        .users
        .membership(principal.user_id, principal.active_org_id)
        .await?
        .ok_or(AuthError::NotMember(principal.active_org_id))?;
    match role {
        Role::Owner | Role::Admin => {}
        Role::Member => return Err(HttpError::Forbidden("owner or admin role required")),
    }
    let now = state.clock.now_utc();
    let updated = state
        .users
        .set_org_language(principal.active_org_id, req.language, now)
        .await?;
    state.language_resolver.invalidate_all();
    Ok((
        StatusCode::OK,
        Json(serde_json::json!({ "default_language": updated })),
    )
        .into_response())
}

#[derive(Debug, Deserialize)]
struct SetOrgRuleRequest {
    /// The rule body. Any of `null`, a missing field, `""`, or a
    /// whitespace-only string clears the rule — the column is nullable
    /// and "no rule" is a meaningful state. The handler folds the empty
    /// / whitespace cases to `None` before parsing so a textarea that
    /// the user blanked out clears the rule instead of returning 400.
    rule: Option<String>,
}

/// `PATCH /me/org/rule` — set or clear the active org's
/// `<organization-rule>` directive.
///
/// Authorization: owner or admin only. Members get 403. The rule shapes
/// every agent the org runs; it is admin-tier configuration, not a
/// per-user preference. Backend is the authority — the FE only hides
/// the editor as a UX nicety.
///
/// Side effect: invalidates the in-process [`OrgRuleResolver`] cache so
/// the agent worker picks up the new value on the next turn. Same
/// whole-keyset invalidation strategy as
/// [`set_org_language`] — cheap, bounded, and rare.
async fn set_org_rule(
    State(state): State<AppState>,
    principal: Principal,
    Json(req): Json<SetOrgRuleRequest>,
) -> Result<Response, HttpError> {
    let role = state
        .users
        .membership(principal.user_id, principal.active_org_id)
        .await?
        .ok_or(AuthError::NotMember(principal.active_org_id))?;
    match role {
        Role::Owner | Role::Admin => {}
        Role::Member => return Err(HttpError::Forbidden("owner or admin role required")),
    }
    // Fold empty / whitespace-only bodies to `None` so the common FE
    // pattern (textarea bound to "") clears the rule instead of 400ing
    // on `ParseError::Empty`. Past that filter, `TryFrom` parses at the
    // boundary (CLAUDE.md §1) and enforces the `MAX_ORG_RULE_BYTES`
    // cap; `HttpError::Parse` -> 400 is already wired.
    let parsed: Option<OrganizationRule> = req
        .rule
        .filter(|s| !s.trim().is_empty())
        .map(OrganizationRule::try_from)
        .transpose()?;
    let now = state.clock.now_utc();
    let updated = state
        .users
        .set_org_rule(principal.active_org_id, parsed, now)
        .await?;
    state.rule_resolver.invalidate_all();
    let updated_str = updated.as_ref().map(|r| r.as_str().to_owned());
    Ok((
        StatusCode::OK,
        Json(serde_json::json!({ "default_rule": updated_str })),
    )
        .into_response())
}
