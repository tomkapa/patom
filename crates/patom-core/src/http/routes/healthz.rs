//! Public ops probes — `/healthz` (liveness) and `/readyz` (readiness).
//! Both stay outside the auth layer so the kubelet can hit them without
//! a cookie.

use std::time::Duration;

use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::get;
use tracing::warn;

use super::super::limits::READYZ_DB_TIMEOUT_MS;
use super::super::state::AppState;

pub(super) fn router() -> Router<AppState> {
    Router::new()
        .route("/healthz", get(|| async { StatusCode::OK }))
        .route("/readyz", get(readyz))
}

async fn readyz(State(state): State<AppState>) -> StatusCode {
    let timeout = Duration::from_millis(READYZ_DB_TIMEOUT_MS);
    match tokio::time::timeout(timeout, state.pool.acquire()).await {
        Ok(Ok(_conn)) => StatusCode::OK,
        Ok(Err(source)) => {
            warn!(cause = "acquire_failed", error = %source, "readyz.db_unavailable");
            StatusCode::SERVICE_UNAVAILABLE
        }
        Err(_elapsed) => {
            warn!(
                cause = "timeout",
                timeout_ms = READYZ_DB_TIMEOUT_MS,
                "readyz.db_unavailable"
            );
            StatusCode::SERVICE_UNAVAILABLE
        }
    }
}
