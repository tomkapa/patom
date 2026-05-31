//! Tracing pipeline: console layer + optional OTLP layer to Honeycomb.
//!
//! Two filters apply:
//! - `OTEL_LOG_LEVEL` (registry-level) gates everything before any layer sees it.
//! - `RUST_LOG` (console-only) further narrows what stderr shows; OTel still
//!   gets the full waterfall.
//!
//! No per-layer filter on the OTel layer: empirically, *any* per-layer filter on
//! `OpenTelemetryLayer` suppresses span creation for `#[instrument]` sites
//! called from spawned tokio tasks (`worker.handle_claim`, `agent.reply`,
//! `execute_tool`) while still letting child events through. Drop noise at the
//! registry level instead.

mod console;
mod event_filter;
pub mod gen_ai;
pub mod log;
mod otlp;
pub mod propagation;

use std::sync::{Mutex, OnceLock};

use opentelemetry_sdk::trace::SdkTracerProvider;
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::Layer;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

const DEFAULT_CONSOLE_FILTER: &str =
    "patom=info,claudius=warn,opentelemetry=info,opentelemetry_sdk=info,opentelemetry_otlp=info";

// EnvFilter directives match crate names, not prefixes — `sqlx=warn` does not
// catch the `sqlx_core` crate, so list both. Same reason `hyper_util` is listed
// alongside `hyper`. `opentelemetry*=info` keeps SDK self-diagnostics (auth
// failures, dropped batches) visible so a misconfigured exporter shows up.
const DEFAULT_GLOBAL_FILTER: &str = "info,patom=debug,claudius=warn,sqlx=warn,sqlx_core=warn,hyper=warn,hyper_util=warn,h2=warn,rustls=warn,reqwest=warn,tower=warn";

/// Flush guard returned from [`init`]. Drop shuts the OTLP provider down so
/// buffered spans flush. Bind in `main`.
#[must_use = "OtelGuard must be held until shutdown so buffered spans flush"]
#[derive(Debug, Default)]
pub struct OtelGuard {
    provider: Option<SdkTracerProvider>,
}

impl Drop for OtelGuard {
    fn drop(&mut self) {
        if let Some(provider) = self.provider.take()
            && let Err(err) = provider.shutdown()
        {
            tracing::warn!(error = %err, "otel.shutdown.failed");
        }
    }
}

/// Initialise the tracing subscriber and (optionally) the OTLP exporter.
/// Exporter failures degrade to console-only with a `warn!`.
pub fn init() -> OtelGuard {
    let global_filter = std::env::var("OTEL_LOG_LEVEL")
        .ok()
        .and_then(|s| EnvFilter::try_new(s).ok())
        .unwrap_or_else(|| EnvFilter::new(DEFAULT_GLOBAL_FILTER));
    let console_filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(DEFAULT_CONSOLE_FILTER));

    let registry = tracing_subscriber::registry()
        .with(global_filter)
        .with(console::layer().with_filter(console_filter));

    match otlp::build_provider() {
        Ok(Some(otlp::Exporter {
            provider,
            tracer,
            backends,
        })) => {
            registry
                .with(tracing_opentelemetry::layer().with_tracer(tracer))
                .init();
            info!(backends = %backends.join(","), "otel.exporter.ready");
            install_emergency_flush(provider.clone());
            OtelGuard {
                provider: Some(provider),
            }
        }
        Ok(None) => {
            registry.init();
            warn!("otel: no backend keys set; OTLP export disabled");
            OtelGuard::default()
        }
        Err(err) => {
            registry.init();
            warn!(error = %err, "otel: exporter init failed; running console-only");
            OtelGuard::default()
        }
    }
}

// Provider handle for emergency-exit paths where Drop will not run
// (e.g. second Ctrl-C calling `std::process::exit`).
static EMERGENCY_FLUSH: OnceLock<Mutex<Option<SdkTracerProvider>>> = OnceLock::new();

fn install_emergency_flush(provider: SdkTracerProvider) {
    let cell = EMERGENCY_FLUSH.get_or_init(|| Mutex::new(None));
    if let Ok(mut slot) = cell.lock() {
        *slot = Some(provider);
    }
}

/// Synchronously flush buffered spans. No-op when OTLP is disabled.
pub fn emergency_flush() {
    let Some(cell) = EMERGENCY_FLUSH.get() else {
        return;
    };
    let Ok(slot) = cell.lock() else { return };
    if let Some(provider) = slot.as_ref()
        && let Err(err) = provider.force_flush()
    {
        tracing::warn!(error = %err, "otel.flush.failed");
    }
}
