//! Per-request principal extraction.
//!
//! [`require_principal`] is the tower middleware applied to every
//! private route subtree. It reads the session cookie, verifies the
//! JWT, looks up the user's membership of the org claim, and stashes a
//! [`Principal`] in request extensions.
//!
//! [`Principal`] is also an axum extractor — handlers take it as a
//! parameter and the framework pulls it back out of the request
//! extensions. A handler reaching this code without the middleware
//! having stashed a principal is a layer-misconfig bug and yields a
//! 500.

use axum::extract::{Request, State};
use axum::middleware::Next;
use axum::response::Response;
use axum_extra::extract::CookieJar;
use tracing::warn;

use crate::auth::{AuthError, Principal};

use super::error::HttpError;
use super::state::AppState;

/// Middleware: extract + validate the session cookie, attach a
/// [`Principal`] to the request, and call the next handler.
///
/// On failure, returns 401 via [`HttpError::Auth`] before the handler
/// is invoked. The handler downstream takes `Principal` via the axum
/// extractor implementation below.
pub(super) async fn require_principal(
    State(state): State<AppState>,
    jar: CookieJar,
    mut request: Request,
    next: Next,
) -> Result<Response, HttpError> {
    let token = jar
        .get(crate::auth::limits::COOKIE_NAME)
        .map(|c| c.value().to_owned())
        .ok_or(AuthError::Unauthenticated)?;
    let claims = state.jwt.verify(&token).map_err(|e| {
        // Verify failures collapse to Unauthenticated at the HTTP layer
        // (don't leak signature vs expiry to the caller); the warn line
        // keeps the differentiation in logs for operators.
        warn!(error = %e, "auth.jwt.verify_failed");
        AuthError::Unauthenticated
    })?;
    let role = if let Some(cached) = state.memberships.lookup(claims.sub, claims.org) {
        cached
    } else {
        let fresh = state
            .users
            .membership(claims.sub, claims.org)
            .await?
            .ok_or(AuthError::NotMember(claims.org))?;
        state.memberships.insert(claims.sub, claims.org, fresh);
        fresh
    };
    let principal = Principal {
        user_id: claims.sub,
        active_org_id: claims.org,
        role,
    };
    request.extensions_mut().insert(principal);
    Ok(next.run(request).await)
}

/// Axum extractor for [`Principal`]. Reads the value the middleware
/// stashed in request extensions; missing means the layer wasn't
/// applied above this handler (programmer error, not user error).
mod extract {
    use axum::extract::FromRequestParts;
    use axum::http::StatusCode;
    use axum::http::request::Parts;
    use axum::response::{IntoResponse, Response};

    use crate::auth::Principal;

    impl<S: Send + Sync> FromRequestParts<S> for Principal {
        type Rejection = MissingPrincipal;

        async fn from_request_parts(
            parts: &mut Parts,
            _state: &S,
        ) -> Result<Self, Self::Rejection> {
            parts
                .extensions
                .get::<Self>()
                .cloned()
                .ok_or(MissingPrincipal)
        }
    }

    /// Returned when a handler tagged with `Principal` was reached
    /// without the auth middleware in front of it. Always a 500 — it's
    /// a bug in the routing graph.
    #[derive(Debug)]
    pub struct MissingPrincipal;

    impl IntoResponse for MissingPrincipal {
        fn into_response(self) -> Response {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "principal missing — auth layer not applied",
            )
                .into_response()
        }
    }
}
