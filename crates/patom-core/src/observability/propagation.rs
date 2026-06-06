//! W3C trace-context propagation across the prompt queue.
//!
//! `send_message` (or the HTTP boundary) calls [`current_traceparent`] to
//! capture the active OTel context as a `traceparent` string. The queue
//! stores it on the row; the worker reads it back at claim time and feeds it
//! to [`apply_parent`] so the `worker.handle_claim` span attaches to the
//! producer's trace instead of starting a disconnected root.
//!
//! No-ops cleanly when the OTel exporter is disabled (no current span
//! context, or the global propagator hasn't been installed): callers see
//! `None` from [`current_traceparent`] and [`apply_parent`] leaves the span
//! as its own root.

use std::collections::HashMap;

use opentelemetry::global;
use opentelemetry::propagation::{Extractor, Injector};
use tracing::Span;
use tracing_opentelemetry::OpenTelemetrySpanExt;

const TRACEPARENT: &str = "traceparent";

/// Capture the current span's OTel context as a W3C `traceparent` string,
/// or `None` when no context is propagating (exporter off, no active span).
pub fn current_traceparent() -> Option<String> {
    let ctx = Span::current().context();
    let mut carrier = HashMapCarrier::default();
    global::get_text_map_propagator(|propagator| {
        propagator.inject_context(&ctx, &mut carrier);
    });
    carrier.0.remove(TRACEPARENT)
}

/// Set `span`'s parent to the trace context encoded in `traceparent`. Silent
/// no-op when `traceparent` is `None` or unparseable — the span keeps its
/// natural parent (typically becomes a new root).
pub fn apply_parent(span: &Span, traceparent: Option<&str>) {
    let Some(tp) = traceparent else {
        return;
    };
    let mut carrier = HashMapCarrier::default();
    carrier.0.insert(TRACEPARENT.to_string(), tp.to_string());
    let parent = global::get_text_map_propagator(|propagator| propagator.extract(&carrier));
    let _ = span.set_parent(parent);
}

#[derive(Default)]
struct HashMapCarrier(HashMap<String, String>);

impl Injector for HashMapCarrier {
    fn set(&mut self, key: &str, value: String) {
        self.0.insert(key.to_string(), value);
    }
}

impl Extractor for HashMapCarrier {
    fn get(&self, key: &str) -> Option<&str> {
        self.0.get(key).map(String::as_str)
    }

    fn keys(&self) -> Vec<&str> {
        self.0.keys().map(String::as_str).collect()
    }
}
