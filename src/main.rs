use anyhow::{Context, Result};
use tokio_util::sync::CancellationToken;

use patom::{Settings, app, observability};

#[tokio::main]
async fn main() -> Result<()> {
    dotenvy::dotenv().ok();
    // Bind the OTel guard to the function scope: `Drop` shuts down the tracer
    // provider after `run_server` returns, flushing buffered spans before the
    // process exits.
    let _otel = observability::init();

    let settings = Settings::load().context("load settings")?;

    // First Ctrl-C asks for graceful shutdown; second Ctrl-C aborts. axum's
    // `with_graceful_shutdown` waits for every in-flight connection to close,
    // so an open SSE stream or hung MCP call would otherwise leave the
    // operator stuck. The escape hatch belongs in `main` because by the time
    // the second signal lands the runtime cannot be trusted to drive a clean
    // exit (CLAUDE.md §6 — assertion-shaped: cannot continue).
    //
    // `cancel` is created up-front so subsystems built inside
    // `build_server` (e.g. the reflection scheduler) can subscribe to
    // the same token and react to Ctrl+C in lockstep.
    let cancel = CancellationToken::new();

    let server = app::build_server(settings, cancel.clone())
        .await
        .context("compose server")?;

    let watch = cancel.clone();
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            tracing::info!("ctrl-c received; shutting down");
            watch.cancel();
        }
        if tokio::signal::ctrl_c().await.is_ok() {
            tracing::warn!("ctrl-c received twice; aborting");
            // `std::process::exit` skips destructors, so `OtelGuard::Drop`
            // never runs and any buffered spans are lost. Force-flush
            // synchronously before we go.
            observability::emergency_flush();
            std::process::exit(130);
        }
    });

    app::run_server(server, cancel).await.context("run server")
}
