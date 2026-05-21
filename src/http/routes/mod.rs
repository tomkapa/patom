//! Per-domain route modules. Each submodule exposes a `router()` that returns a
//! `Router<AppState>` for its slice of the wire surface; this module merges them and
//! attaches global middleware once.
//!
//! Three tiers: ops probes (`/healthz`, `/readyz`) and OAuth landing
//! pads (`/auth/google/*`, `/mcp-oauth/*`) at root because external
//! systems hold those paths; JSON nested under `/api/*`; SPA shell
//! served as a `ServeDir` fallback with `index.html` for unknown
//! paths.

mod agents;
mod auth;
mod healthz;
mod mcp;
mod me;
mod memory;
mod prompts;
mod threads;

use axum::Router;
use axum::middleware;
use tower_http::limit::RequestBodyLimitLayer;
use tower_http::services::{ServeDir, ServeFile};
use tower_http::trace::TraceLayer;

use super::auth_layer::require_principal;
use super::csrf::require_csrf;
use super::limits::REQUEST_BODY_LIMIT_BYTES;
use super::state::AppState;

pub fn router(state: AppState) -> Router {
    let public = Router::new()
        .merge(auth::router())
        .merge(healthz::router())
        // The MCP OAuth callback runs without a session cookie — the
        // browser is returning from the vendor. CSRF is provided by the
        // PKCE state column, not the double-submit cookie.
        .merge(mcp::oauth_callback_router())
        // Slack webhook + OAuth callback also sit outside the cookie
        // gate. Slack signs each webhook (HMAC-SHA256) and the OAuth
        // callback validates an HMAC-signed state token.
        .merge(crate::slack::events::router())
        .merge(crate::slack::interactions::router())
        .merge(crate::slack::oauth::public_router());

    let private = Router::new()
        .merge(prompts::router())
        .merge(agents::router())
        .merge(mcp::router())
        .merge(memory::router())
        .merge(threads::router())
        .merge(me::router())
        // Slack install endpoint — signed-in user only.
        .merge(crate::slack::oauth::private_router())
        // CSRF guards every state-changing request inside the
        // authenticated subtree. Order matters: it runs AFTER
        // `require_principal` so the public subtree is never reached
        // (OAuth login/callback have no cookie to compare yet).
        .route_layer(middleware::from_fn(require_csrf))
        // route_layer is the only correct place for auth middleware —
        // applying it via `.layer` would also wrap the public subtree
        // below and reject `/auth/google/*` with 401.
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            require_principal,
        ));

    // Misses fall to `index.html` so React Router resolves deep links.
    // Missing files at boot are intentionally not validated — surfacing
    // as 404s catches a broken deploy at the smoke test instead of at
    // startup, and lets the BE run without a built FE in dev.
    let index_html = state.web_dist.join("index.html");
    let spa_fallback = ServeDir::new(&state.web_dist).not_found_service(ServeFile::new(index_html));

    Router::new()
        .merge(public)
        .nest("/api", private)
        .fallback_service(spa_fallback)
        .with_state(state)
        .layer(RequestBodyLimitLayer::new(REQUEST_BODY_LIMIT_BYTES))
        .layer(TraceLayer::new_for_http())
}
