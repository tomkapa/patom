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
mod models;
mod org;
pub(super) mod prompts;
mod threads;
pub(super) mod turns;
mod uploads;

use axum::Router;
use axum::http::{HeaderName, HeaderValue, Method, header};
use axum::middleware;
use tower_http::classify::{ServerErrorsAsFailures, SharedClassifier};
use tower_http::cors::{AllowOrigin, CorsLayer};
use tower_http::limit::RequestBodyLimitLayer;
use tower_http::services::{ServeDir, ServeFile};
use tower_http::trace::{DefaultMakeSpan, TraceLayer};
use tracing::Level;

use super::auth_layer::require_principal;
use super::csrf::require_csrf;
use super::limits::REQUEST_BODY_LIMIT_BYTES;
use super::state::AppState;

/// Build the credentialed CORS layer for the `/api` subtree from the
/// configured origin allowlist, or `None` when no origins are set.
///
/// `None` preserves the same-origin-only default (the app SPA shares an
/// origin with the API, so it needs no CORS). `Some` is used so the apex
/// marketing site (`https://patom.app`) can read `app.patom.app/api/me`
/// with `credentials: 'include'` and toggle its Sign in ↔ Open app nav.
///
/// Credentials forbid a wildcard origin, so the allowlist is an explicit
/// list. The allowed header set mirrors the SPA's contract: JSON bodies
/// (`Content-Type`) and the CSRF echo header
/// ([`crate::auth::limits::CSRF_HEADER_NAME`], lowercased — CORS header
/// matching is case-insensitive).
fn build_cors_layer(origins: &[String]) -> Option<CorsLayer> {
    if origins.is_empty() {
        return None;
    }
    let parsed: Vec<HeaderValue> = origins
        .iter()
        .filter_map(|o| HeaderValue::from_str(o).ok())
        .collect();
    // Every origin came from `parse_origin` (canonical ASCII), so this
    // only trips on a programmer error upstream.
    assert_eq!(
        parsed.len(),
        origins.len(),
        "validated CORS origin failed to parse as a header value"
    );
    assert!(!parsed.is_empty());
    Some(
        CorsLayer::new()
            .allow_origin(AllowOrigin::list(parsed))
            .allow_credentials(true)
            .allow_methods([Method::GET, Method::POST])
            .allow_headers([
                header::CONTENT_TYPE,
                HeaderName::from_static("x-csrf-token"),
            ]),
    )
}

/// HTTP request-tracing layer.
///
/// The request span is pinned to `INFO`. `DefaultMakeSpan` defaults to
/// `DEBUG`, and the span's target is the `tower_http` crate — which is not
/// named in `observability::DEFAULT_GLOBAL_FILTER`, so it inherits the `info`
/// floor. At `DEBUG` the span is therefore never enabled, which (a) leaves
/// every `#[instrument]` work span parentless (no per-request root) and (b)
/// silently drops the `http.error.5xx` event that `HttpError::into_response`
/// emits, because the OTel bridge discards events fired with no active span.
/// `INFO` clears the floor so every request is rooted and every 5xx is
/// attributable.
fn trace_layer() -> TraceLayer<SharedClassifier<ServerErrorsAsFailures>, DefaultMakeSpan> {
    TraceLayer::new_for_http().make_span_with(DefaultMakeSpan::new().level(Level::INFO))
}

pub fn router(state: AppState) -> Router {
    let public = Router::new()
        .merge(auth::router())
        .merge(healthz::router())
        // The MCP OAuth callback runs without a session cookie — the
        // browser is returning from the vendor. CSRF is provided by the
        // PKCE state column, not the double-submit cookie.
        .merge(mcp::oauth_callback_router())
        // Slack-initiated MCP wiring lands here — also no cookie session,
        // auth is the `connect_link`-signed token in the query string.
        .merge(mcp::slack_connect_router())
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
        .merge(models::router())
        .merge(threads::router())
        .merge(turns::router())
        .merge(me::router())
        .merge(org::router())
        .merge(uploads::router())
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

    // CORS sits OUTERMOST relative to the auth/CSRF route layers: a
    // preflight `OPTIONS` carries no session cookie and no CSRF header,
    // so it must be short-circuited by the CORS layer before
    // `require_principal` would 401 it. `.layer` wraps the already-built
    // private router (including its route layers), giving that ordering.
    // `None` (no allowlist) leaves the subtree same-origin only.
    let private = match build_cors_layer(&state.cors_allowed_origins) {
        Some(cors) => private.layer(cors),
        None => private,
    };

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
        .layer(trace_layer())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use axum::routing::get;
    use tower::ServiceExt;
    use tracing_subscriber::EnvFilter;
    use tracing_subscriber::layer::SubscriberExt;

    /// Records the `(name, level)` of every span the subscriber enables, so a
    /// test can assert which spans survive a given filter floor.
    #[derive(Clone, Default)]
    struct SpanCapture(std::sync::Arc<std::sync::Mutex<Vec<(&'static str, Level)>>>);

    impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for SpanCapture {
        fn on_new_span(
            &self,
            attrs: &tracing::span::Attributes<'_>,
            _id: &tracing::span::Id,
            _ctx: tracing_subscriber::layer::Context<'_, S>,
        ) {
            let meta = attrs.metadata();
            self.0
                .lock()
                .expect("span capture mutex")
                .push((meta.name(), *meta.level()));
        }
    }

    // Regression for the dropped `http.error.5xx` event: the HTTP request span
    // must open at INFO so it clears the `info` global filter floor
    // (observability::DEFAULT_GLOBAL_FILTER). At DefaultMakeSpan's DEBUG it is
    // filtered out entirely — leaving work spans parentless and discarding the
    // span-less 5xx error event the OTel bridge can't attach.
    #[tokio::test]
    async fn request_span_opens_at_info_above_filter_floor() {
        let capture = SpanCapture::default();
        let sink = capture.0.clone();
        // Mirror production's floor. A DEBUG request span is filtered here and
        // never reaches the capture layer; an INFO one passes.
        let subscriber = tracing_subscriber::registry()
            .with(EnvFilter::new("info"))
            .with(capture);

        let router = Router::new()
            .route("/probe", get(|| async { "ok" }))
            .layer(trace_layer());

        let req = Request::builder()
            .uri("/probe")
            .body(Body::empty())
            .expect("request");
        {
            let _guard = tracing::subscriber::set_default(subscriber);
            let res = router.oneshot(req).await.expect("response");
            assert_eq!(res.status(), StatusCode::OK);
        }

        let spans = sink.lock().expect("span capture mutex");
        let request_span = spans.iter().find(|(name, _)| *name == "request");
        assert!(
            request_span.is_some(),
            "request span missing under info floor; spans seen: {spans:?}"
        );
        assert_eq!(
            request_span.expect("request span present").1,
            Level::INFO,
            "request span must be INFO to clear the info filter floor"
        );
    }

    /// Minimal router applying the real `build_cors_layer` output to a
    /// stand-in `/api/me`. Exercises the production helper without a
    /// DB-backed `AppState` (preflight is pre-auth anyway).
    fn cors_router(origins: &[String]) -> Router {
        let r = Router::new().route("/api/me", get(|| async { "ok" }));
        match build_cors_layer(origins) {
            Some(cors) => r.layer(cors),
            None => r,
        }
    }

    #[tokio::test]
    async fn preflight_echoes_allowed_origin_with_credentials() {
        let router = cors_router(&["https://patom.app".to_string()]);
        let req = Request::builder()
            .method(Method::OPTIONS)
            .uri("/api/me")
            .header("origin", "https://patom.app")
            .header("access-control-request-method", "POST")
            .header("access-control-request-headers", "x-csrf-token")
            .body(Body::empty())
            .expect("request");
        let res = router.oneshot(req).await.expect("response");
        // Preflight is short-circuited (no auth) with a 2xx.
        assert!(res.status().is_success());
        let h = res.headers();
        assert_eq!(
            h.get("access-control-allow-origin").expect("acao"),
            "https://patom.app"
        );
        assert_eq!(
            h.get("access-control-allow-credentials").expect("acac"),
            "true"
        );
        let methods = h
            .get("access-control-allow-methods")
            .expect("methods")
            .to_str()
            .expect("ascii");
        assert!(methods.contains("POST"), "methods: {methods}");
        let headers = h
            .get("access-control-allow-headers")
            .expect("headers")
            .to_str()
            .expect("ascii")
            .to_ascii_lowercase();
        assert!(headers.contains("x-csrf-token"), "headers: {headers}");
    }

    #[tokio::test]
    async fn disallowed_origin_is_not_echoed() {
        let router = cors_router(&["https://patom.app".to_string()]);
        let req = Request::builder()
            .method(Method::GET)
            .uri("/api/me")
            .header("origin", "https://evil.test")
            .body(Body::empty())
            .expect("request");
        let res = router.oneshot(req).await.expect("response");
        assert_eq!(res.status(), StatusCode::OK);
        assert!(
            res.headers().get("access-control-allow-origin").is_none(),
            "evil origin must not be echoed"
        );
    }

    #[test]
    fn empty_allowlist_disables_cors() {
        assert!(build_cors_layer(&[]).is_none());
        assert!(build_cors_layer(&["https://patom.app".to_string()]).is_some());
    }
}
