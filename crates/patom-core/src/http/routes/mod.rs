//! Per-domain route modules. Each submodule exposes a `router()` that returns a
//! `Router<AppState>` for its slice of the wire surface; this module merges them and
//! attaches global middleware once.
//!
//! Three tiers: ops probes (`/healthz`, `/readyz`) and OAuth landing
//! pads (`/auth/oidc/*`, `/mcp-oauth/*`) at root because external
//! systems hold those paths; JSON nested under `/api/*`; SPA shell
//! served as a `ServeDir` fallback with `index.html` for unknown
//! paths.

mod agents;
mod auth;
mod channels;
mod healthz;
mod mcp;
mod me;
mod memory;
mod models;
mod org;
pub(super) mod prompts;
mod provider_credentials;
mod scheduling;
mod threads;
pub(super) mod turns;
mod uploads;

use std::sync::Arc;

use axum::Router;
use axum::http::{HeaderName, HeaderValue, Method, header};
use axum::middleware;
use tower::util::BoxCloneSyncService;
use tower_http::classify::{ServerErrorsAsFailures, SharedClassifier};
use tower_http::cors::{AllowOrigin, CorsLayer};
use tower_http::limit::RequestBodyLimitLayer;
use tower_http::services::ServeDir;
use tower_http::trace::{MakeSpan, TraceLayer};

use super::auth_layer::{require_principal, require_user};
use super::csrf::{require_csrf, require_trusted_origin};
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
/// The root span is emitted under the `patom::http` target (see
/// [`PatomHttpMakeSpan`]) rather than `tower_http`'s default. The deployed
/// `observability::DEFAULT_GLOBAL_FILTER` carries a `tower=warn` directive,
/// and `EnvFilter` matches targets by raw prefix (`directive.rs`:
/// `meta.target().starts_with(..)`), so `tower` *also* captures
/// `tower_http::trace`. A span made by `DefaultMakeSpan` (target `tower_http`)
/// is therefore pinned to WARN+ and filtered out at any sane level — which
/// leaves every `#[instrument]` work span parentless AND silently drops the
/// `http.error.5xx` event `HttpError::into_response` emits, because the OTel
/// bridge discards events fired with no active span. Rooting under
/// `patom::http` rides the `patom` directive instead, so the INFO span is
/// always enabled: every request is rooted and every 5xx is attributable.
fn trace_layer() -> TraceLayer<SharedClassifier<ServerErrorsAsFailures>, PatomHttpMakeSpan> {
    TraceLayer::new_for_http().make_span_with(PatomHttpMakeSpan)
}

/// [`MakeSpan`] that roots the per-request span under the `patom::http`
/// target at INFO so it survives the `tower=warn` prefix in the global
/// filter. See [`trace_layer`] for why the target — not the level — is the
/// thing that matters.
#[derive(Clone, Copy, Debug)]
struct PatomHttpMakeSpan;

impl<B> MakeSpan<B> for PatomHttpMakeSpan {
    fn make_span(&mut self, request: &axum::http::Request<B>) -> tracing::Span {
        tracing::info_span!(
            target: "patom::http",
            "request",
            http.request.method = %request.method(),
            url.path = request.uri().path(),
        )
    }
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
        // Lark-initiated MCP wiring (same no-cookie, signed-token model).
        .merge(mcp::lark_connect_router())
        // Lark interactive-approval card callback — no cookie; verified per
        // request with the app's Encrypt Key + Verification Token (#214).
        .merge(crate::lark::card_actions::router())
        // Discord-initiated MCP wiring (same no-cookie, signed-token model).
        .merge(mcp::discord_connect_router())
        // Slack webhook + OAuth callback also sit outside the cookie
        // gate. Slack signs each webhook (HMAC-SHA256) and the OAuth
        // callback validates an HMAC-signed state token.
        .merge(crate::slack::events::router())
        .merge(crate::slack::interactions::router())
        .merge(crate::slack::oauth::public_router())
        // "Set up Patom" button target — auth is the signed link token in
        // the query string, no cookie session yet.
        .merge(crate::slack::identity_routes::start_router());

    let private = Router::new()
        .merge(prompts::router())
        .merge(agents::router())
        .merge(channels::router())
        .merge(scheduling::router())
        .merge(mcp::router())
        .merge(memory::router())
        .merge(models::router())
        .merge(threads::router())
        .merge(turns::router())
        .merge(me::router())
        .merge(org::router())
        .merge(provider_credentials::router())
        .merge(uploads::router())
        // Slack install endpoint — signed-in user only.
        .merge(crate::slack::oauth::private_router())
        // Slack identity unlink — signed-in member only.
        .merge(crate::slack::identity_routes::unlink_router())
        // Lark bot registration — admin-only (signed-in member).
        .merge(crate::lark::admin_routes::private_router())
        // Discord bot registration — admin-only (signed-in member).
        .merge(crate::discord::admin_routes::private_router())
        // Origin/Referer check — the second CSRF layer (alongside the
        // double-submit token below). Rejects state-changing requests
        // naming an untrusted origin. Needs `AppState` for the trusted
        // origin set, so it's `from_fn_with_state`.
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            require_trusted_origin,
        ))
        // CSRF guards every state-changing request inside the
        // authenticated subtree. Order matters: it runs AFTER
        // `require_principal` so the public subtree is never reached
        // (OAuth login/callback have no cookie to compare yet).
        .route_layer(middleware::from_fn(require_csrf))
        // route_layer is the only correct place for auth middleware —
        // applying it via `.layer` would also wrap the public subtree
        // below and reject `/auth/oidc/*` with 401.
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            require_principal,
        ));

    // Onboarding tier — same CSRF + origin guards as `private`, but a
    // lighter `require_user` auth that accepts an org-less session (a
    // cloud user who hasn't created a workspace yet). `GET /me` and
    // `POST /me/orgs` live here. Merged into the `/api` nest below; its
    // route_layers wrap only its own routes.
    let onboarding = Router::new()
        .merge(me::onboarding_router())
        // Slack identity-link completion runs right after login, when a
        // brand-new user is still org-less — so it needs `require_user`,
        // not `require_principal` (which would 401 an org-less session).
        .merge(crate::slack::identity_routes::complete_router())
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            require_trusted_origin,
        ))
        .route_layer(middleware::from_fn(require_csrf))
        .route_layer(middleware::from_fn_with_state(state.clone(), require_user));

    let private = private.merge(onboarding);

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

    // Real files from `web_dist`; the runtime-config-injected `index_html` for
    // `/` and every SPA deep link (see [`build_spa_fallback`]).
    let spa_fallback = build_spa_fallback(&state.web_dist, Arc::clone(&state.index_html));

    Router::new()
        .merge(public)
        .nest("/api", private)
        .fallback_service(spa_fallback)
        .with_state(state)
        .layer(RequestBodyLimitLayer::new(REQUEST_BODY_LIMIT_BYTES))
        .layer(trace_layer())
}

/// SPA fallback service: serves real files from `web_dist`, and the
/// runtime-config-injected `index_html` (HTTP 200) for `/` and every
/// client-side route.
///
/// Two load-bearing choices:
/// - `append_index_html_on_directories(false)`: by default `ServeDir` serves
///   `web_dist/index.html` for `GET /` directly, which bypasses `index_html`
///   and ships the SPA *without* `window.__PATOM_CONFIG__` — so the canonical
///   root load would silently miss analytics config while only deep links got
///   it. Disabling it routes `/` through the fallback too.
/// - `.fallback()` (not `.not_found_service()`): the latter wraps the fallback
///   in `SetStatus(404)`, so the SPA shell would be served with a 404 status on
///   `/` and every deep link. `.fallback()` preserves the 200 the shell sets,
///   which is the correct status for an SPA entry document.
fn build_spa_fallback(
    web_dist: &std::path::Path,
    index_html: Arc<str>,
) -> ServeDir<
    BoxCloneSyncService<
        axum::http::Request<axum::body::Body>,
        axum::http::Response<axum::body::Body>,
        std::convert::Infallible,
    >,
> {
    let index_svc = BoxCloneSyncService::new(tower::service_fn(
        move |_req: axum::http::Request<axum::body::Body>| {
            let html = Arc::clone(&index_html);
            async move {
                Ok::<_, std::convert::Infallible>(
                    axum::http::Response::builder()
                        .header(axum::http::header::CONTENT_TYPE, "text/html; charset=utf-8")
                        .body(axum::body::Body::from(html.as_bytes().to_vec()))
                        .expect("invariant: response builder with known-valid header cannot fail"),
                )
            }
        },
    ));
    ServeDir::new(web_dist)
        .append_index_html_on_directories(false)
        .fallback(index_svc)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use axum::routing::get;
    use tower::ServiceExt;
    use tracing::Level;
    use tracing_subscriber::EnvFilter;
    use tracing_subscriber::layer::SubscriberExt;

    /// One captured span: `(name, target, level)`. The target is what the
    /// `tower=warn` prefix turns on, so the test asserts on it.
    type CapturedSpan = (&'static str, &'static str, Level);

    /// Records every span the subscriber enables, so a test can assert which
    /// spans survive a given filter and on which target.
    #[derive(Clone, Default)]
    struct SpanCapture(std::sync::Arc<std::sync::Mutex<Vec<CapturedSpan>>>);

    impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for SpanCapture {
        fn on_new_span(
            &self,
            attrs: &tracing::span::Attributes<'_>,
            _id: &tracing::span::Id,
            _ctx: tracing_subscriber::layer::Context<'_, S>,
        ) {
            let meta = attrs.metadata();
            self.0.lock().expect("span capture mutex").push((
                meta.name(),
                meta.target(),
                *meta.level(),
            ));
        }
    }

    // Regression for the dropped `http.error.5xx` event. The HTTP request span
    // must survive the *real* `DEFAULT_GLOBAL_FILTER`, which carries
    // `tower=warn`. EnvFilter matches targets by raw prefix, so `tower` also
    // captures `tower_http::trace` — a `DefaultMakeSpan` span (target
    // `tower_http`) is pinned to WARN+ and filtered out at INFO, orphaning work
    // spans and discarding the span-less 5xx error event the OTel bridge can't
    // attach. Rooting under `patom::http` (PatomHttpMakeSpan) is what makes it
    // survive. With the default tower_http target this test fails (no span at
    // all is captured).
    #[tokio::test]
    async fn request_span_survives_production_global_filter() {
        let capture = SpanCapture::default();
        let sink = capture.0.clone();
        // The REAL production directives — notably `tower=warn`. If EnvFilter
        // prefix-matches `tower` against the `tower_http::trace` target, the
        // request span is pinned to WARN+ and filtered out at INFO.
        let subscriber = tracing_subscriber::registry()
            .with(EnvFilter::new(crate::observability::DEFAULT_GLOBAL_FILTER))
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
        // Assert the TARGET, not just the name: a `tower_http`-targeted span of
        // the same name is exactly what `tower=warn` filters out, so pinning
        // `patom::http` is the contract this PR restores.
        let request_span = spans
            .iter()
            .find(|(name, target, _)| *name == "request" && *target == "patom::http");
        assert!(
            request_span.is_some(),
            "request span missing under the patom::http target (the production \
             tower=warn prefix filters the tower_http target); spans seen: {spans:?}"
        );
        assert_eq!(
            request_span.expect("request span present").2,
            Level::INFO,
            "request span must be exported at INFO under the production filter"
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

    async fn body_string(res: axum::http::Response<Body>) -> String {
        let bytes = axum::body::to_bytes(res.into_body(), 64 * 1024)
            .await
            .expect("collect body");
        String::from_utf8(bytes.to_vec()).expect("utf8 body")
    }

    // Regression: `GET /` must serve the runtime-config-INJECTED shell, not the
    // raw on-disk index.html. `ServeDir` serves `index.html` for `/` by
    // default, which would silently ship the SPA without
    // `window.__PATOM_CONFIG__` (analytics off for every root load). The
    // `append_index_html_on_directories(false)` in `build_spa_fallback` is what
    // routes `/` — and every SPA deep link — through the injected shell, while
    // real asset files are still served straight from disk.
    #[tokio::test]
    async fn root_and_deep_links_serve_injected_shell_not_disk_index() {
        let dir = std::env::temp_dir().join(format!("patom_spa_fallback_{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mk tmp dir");
        std::fs::write(
            dir.join("index.html"),
            "<html><head></head><body>RAW_DISK</body></html>",
        )
        .expect("write index.html");
        std::fs::write(dir.join("app.js"), "console.log('real asset');").expect("write asset");

        let injected: Arc<str> = Arc::from(
            "<html><head><script>window.__PATOM_CONFIG__={\"posthogKey\":\"phc_x\"};\
             </script></head><body>INJECTED</body></html>",
        );
        let app = Router::new().fallback_service(build_spa_fallback(&dir, injected));

        // `/` → injected shell, never the disk file.
        let root = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/")
                    .body(Body::empty())
                    .expect("req"),
            )
            .await
            .expect("resp");
        assert_eq!(root.status(), StatusCode::OK);
        let root = body_string(root).await;
        assert!(
            root.contains("__PATOM_CONFIG__"),
            "root must serve injected shell: {root}"
        );
        assert!(
            !root.contains("RAW_DISK"),
            "root must not serve the raw disk index.html: {root}"
        );

        // SPA deep link (no such file) → injected shell.
        let deep = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/threads/abc")
                    .body(Body::empty())
                    .expect("req"),
            )
            .await
            .expect("resp");
        assert!(body_string(deep).await.contains("__PATOM_CONFIG__"));

        // A real asset file → served from disk by ServeDir, not the shell.
        let asset = app
            .oneshot(
                Request::builder()
                    .uri("/app.js")
                    .body(Body::empty())
                    .expect("req"),
            )
            .await
            .expect("resp");
        assert_eq!(asset.status(), StatusCode::OK);
        assert!(body_string(asset).await.contains("real asset"));

        std::fs::remove_dir_all(&dir).ok();
    }
}
