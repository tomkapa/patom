//! Discovery is two bounded fetches: RFC 9728 protected-resource metadata
//! (advertises the issuer) → RFC 8414 authorization-server metadata
//! (advertises the endpoints). One hop each; no fallback probes, no
//! delegation chase. Each leg is timeout-bounded and size-bounded so a
//! poisoned response cannot bloat the OAuth-start handler.

#![allow(clippy::expect_used)]

use axum::Router;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;
use patom_rs::mcp::oauth::discover_authorization_server;
use reqwest::Client;
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

/// RAII guard around a spawned axum server task. Aborts on drop so a
/// test that bails (panic, early `?`) doesn't leak the task into the
/// runtime — CLAUDE.md §7: "Floating tasks are banned: `tokio::spawn(...)`
/// whose `JoinHandle` is dropped."
struct ServerGuard(JoinHandle<()>);

impl Drop for ServerGuard {
    fn drop(&mut self) {
        self.0.abort();
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn discovers_endpoints_via_two_well_known_hops() {
    // Reusable stub: the two well-known paths are wired up; the AS doc
    // self-identifies with the stub's own origin so the §2.4 self-check
    // passes.
    let (base, _guard) = spawn_stub().await;

    let http = Client::new();
    let meta = discover_authorization_server(&http, &format!("{base}/mcp/v1"))
        .await
        .expect("discovery should succeed");

    assert_eq!(meta.issuer, base);
    assert_eq!(meta.authorization_endpoint, format!("{base}/authorize"));
    assert_eq!(meta.token_endpoint, format!("{base}/token"));
    assert_eq!(meta.registration_endpoint, Some(format!("{base}/register")));
    let methods = meta
        .token_endpoint_auth_methods_supported
        .expect("methods advertised");
    assert!(methods.iter().any(|m| m == "none"));
    assert!(methods.iter().any(|m| m == "client_secret_post"));
}

#[tokio::test(flavor = "multi_thread")]
async fn accepts_trailing_slash_drift_between_prm_and_as() {
    // Google's PRM advertises `<base>/` (trailing slash) while the AS
    // doc at that issuer self-declares `<base>` (no slash). The
    // RFC 8414 §2.4 self-consistency check must canonicalise both
    // sides so this real-world drift doesn't break discovery.
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local_addr");
    let base = format!("http://{addr}");
    let prm_iss = format!("{base}/"); // PRM advertises with slash
    let as_iss = base.clone(); // AS self-declares without
    let prm_for_route = prm_iss.clone();
    let as_for_route = as_iss.clone();
    let app = Router::new()
        .route(
            "/.well-known/oauth-protected-resource/mcp/v1",
            get(move || {
                let iss = prm_for_route.clone();
                async move {
                    (
                        [("content-type", "application/json")],
                        format!(r#"{{"authorization_servers":["{iss}"]}}"#),
                    )
                        .into_response()
                }
            }),
        )
        .route(
            "/.well-known/oauth-authorization-server",
            get(move || {
                let iss = as_for_route.clone();
                async move {
                    (
                        [("content-type", "application/json")],
                        format!(
                            r#"{{
                                "issuer":"{iss}",
                                "authorization_endpoint":"{iss}/authorize",
                                "token_endpoint":"{iss}/token"
                            }}"#
                        ),
                    )
                        .into_response()
                }
            }),
        );
    let _guard = ServerGuard(tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    }));

    let http = Client::new();
    let meta = discover_authorization_server(&http, &format!("{base}/mcp/v1"))
        .await
        .expect("discovery should tolerate trailing-slash drift");
    // Returned issuer is canonicalised (no trailing slash).
    assert_eq!(meta.issuer, base);
}

#[tokio::test(flavor = "multi_thread")]
async fn rejects_when_well_known_404s() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local_addr");
    let app = Router::new().fallback(get(|| async { StatusCode::NOT_FOUND }));
    let _guard = ServerGuard(tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    }));

    let http = Client::new();
    let err = discover_authorization_server(&http, &format!("http://{addr}/mcp/v1"))
        .await
        .expect_err("404 must surface as a discovery error");
    let msg = err.to_string();
    assert!(msg.contains("404"), "expected 404 in error, got: {msg}");
}

#[tokio::test(flavor = "multi_thread")]
async fn rejects_when_as_metadata_issuer_mismatches() {
    // PRM declares issuer X, AS metadata self-claims as Y — no
    // delegation chase, just reject.
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local_addr");
    let lying_issuer = format!("http://{addr}");
    let lying_for_route = lying_issuer.clone();
    let app = Router::new()
        .route(
            "/.well-known/oauth-protected-resource/mcp/v1",
            get(move || {
                let iss = lying_for_route.clone();
                async move {
                    (
                        [("content-type", "application/json")],
                        format!(r#"{{"authorization_servers":["{iss}"]}}"#),
                    )
                }
            }),
        )
        .route(
            "/.well-known/oauth-authorization-server",
            get(|| async {
                (
                    [("content-type", "application/json")],
                    r#"{
                        "issuer":"https://attacker.example",
                        "authorization_endpoint":"https://attacker.example/authorize",
                        "token_endpoint":"https://attacker.example/token"
                    }"#,
                )
            }),
        );
    let _guard = ServerGuard(tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    }));

    let http = Client::new();
    let err = discover_authorization_server(&http, &format!("http://{addr}/mcp/v1"))
        .await
        .expect_err("delegated-issuer doc must be rejected");
    let msg = err.to_string();
    assert!(
        msg.contains("issuer mismatch"),
        "expected issuer-mismatch error, got: {msg}"
    );
}

/// Stub that serves both well-knowns with the AS doc self-claiming the
/// stub's own origin. Routed via shared state so the AS doc can plug in
/// the actual `http://127.0.0.1:<port>` once `axum::serve` has bound.
async fn spawn_stub() -> (String, ServerGuard) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local_addr");
    let base = format!("http://{addr}");
    let prm_base = base.clone();
    let as_base = base.clone();
    let app = Router::new()
        .route(
            "/.well-known/oauth-protected-resource/mcp/v1",
            get(move || {
                let iss = prm_base.clone();
                async move {
                    (
                        [("content-type", "application/json")],
                        format!(r#"{{"authorization_servers":["{iss}"]}}"#),
                    )
                        .into_response()
                }
            }),
        )
        .route(
            "/.well-known/oauth-authorization-server",
            get(move || {
                let iss = as_base.clone();
                async move {
                    (
                        [("content-type", "application/json")],
                        format!(
                            r#"{{
                                "issuer":"{iss}",
                                "authorization_endpoint":"{iss}/authorize",
                                "token_endpoint":"{iss}/token",
                                "registration_endpoint":"{iss}/register",
                                "token_endpoint_auth_methods_supported":["none","client_secret_post"]
                            }}"#
                        ),
                    )
                        .into_response()
                }
            }),
        );
    let handle = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (base, ServerGuard(handle))
}
