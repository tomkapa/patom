//! OTLP/HTTP exporters for Honeycomb and Langfuse. Built once at startup (CLAUDE.md §9).
//!
//! One `SdkTracerProvider` fans spans out to up to two backends, each with its
//! own `BatchSpanProcessor`. Honeycomb is enabled when `HONEYCOMB_API_KEY` is
//! set; Langfuse is enabled when both `LANGFUSE_PUBLIC_KEY` and
//! `LANGFUSE_SECRET_KEY` are set. Returns `Ok(None)` when neither backend is
//! configured so the caller can boot console-only. Errors are reserved for
//! malformed config; init does not crash.

use std::collections::HashMap;
use std::time::Duration;

use base64::Engine as _;
use opentelemetry::KeyValue;
use opentelemetry::global;
use opentelemetry::propagation::TextMapCompositePropagator;
use opentelemetry::trace::TracerProvider as _;
use opentelemetry_otlp::{Protocol, SpanExporter, WithExportConfig, WithHttpConfig};
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::propagation::{BaggagePropagator, TraceContextPropagator};
use opentelemetry_sdk::trace::{BatchSpanProcessor, SdkTracerProvider, Tracer};
use opentelemetry_semantic_conventions::resource as semres;
use uuid::Uuid;

use super::event_filter::EventFilteringProcessor;
use super::gen_ai::{EVENT_NAME_INPUT, EVENT_NAME_OUTPUT};

/// Span events whose `event_name` attribute matches one of these are dropped
/// before OTLP export. The captured-content payload already rides on the span
/// as a `gen_ai.*.messages` attribute (see `gen_ai.rs`); the duplicate span
/// event is kept on the console layer for local dev only.
const DROPPED_EVENT_NAMES: &[&str] = &[EVENT_NAME_INPUT, EVENT_NAME_OUTPUT];

const SERVICE_NAME: &str = env!("CARGO_PKG_NAME");
const SERVICE_VERSION: &str = env!("CARGO_PKG_VERSION");

// `.with_endpoint(...)` posts to the URL verbatim — unlike the
// `OTEL_EXPORTER_OTLP_ENDPOINT` env-var path through the SDK, it does **not**
// auto-append `/v1/traces`. Operators set `*_BASE_URL` (host or OTLP base);
// `ensure_traces_path` fills in the trace-signal suffix. Posting to a bare
// OTLP base returns HTTP 200 from Langfuse but silently drops every span.
const HONEYCOMB_DEFAULT_BASE: &str = "https://api.honeycomb.io";
const LANGFUSE_DEFAULT_BASE: &str = "https://us.cloud.langfuse.com/api/public/otel";
const TRACES_PATH: &str = "/v1/traces";
const EXPORT_TIMEOUT: Duration = Duration::from_secs(10);

pub(super) struct Exporter {
    pub provider: SdkTracerProvider,
    pub tracer: Tracer,
    /// Names of the OTLP backends actually wired up — `"honeycomb"`,
    /// `"langfuse"`, or both. Stable, low-cardinality strings used directly
    /// as a log attribute (CLAUDE.md §2).
    pub backends: Vec<&'static str>,
}

pub(super) fn build_provider() -> Result<Option<Exporter>, OtelInitError> {
    let honeycomb = build_honeycomb_exporter()?;
    let langfuse = build_langfuse_exporter()?;

    if honeycomb.is_none() && langfuse.is_none() {
        return Ok(None);
    }

    let resource = Resource::builder()
        .with_service_name(SERVICE_NAME)
        .with_attribute(KeyValue::new(semres::SERVICE_VERSION, SERVICE_VERSION))
        .with_attribute(KeyValue::new(
            semres::SERVICE_INSTANCE_ID,
            Uuid::new_v4().to_string(),
        ))
        .build();

    let mut builder = SdkTracerProvider::builder().with_resource(resource);
    let mut backends: Vec<&'static str> = Vec::with_capacity(2);

    if let Some(exporter) = honeycomb {
        builder = builder.with_span_processor(filtered_batch(exporter));
        backends.push("honeycomb");
    }
    if let Some(exporter) = langfuse {
        builder = builder.with_span_processor(filtered_batch(exporter));
        backends.push("langfuse");
    }

    let provider = builder.build();

    global::set_text_map_propagator(TextMapCompositePropagator::new(vec![
        Box::new(TraceContextPropagator::new()),
        Box::new(BaggagePropagator::new()),
    ]));
    global::set_tracer_provider(provider.clone());

    let tracer = provider.tracer(SERVICE_NAME);
    Ok(Some(Exporter {
        provider,
        tracer,
        backends,
    }))
}

fn build_honeycomb_exporter() -> Result<Option<SpanExporter>, OtelInitError> {
    let Ok(api_key) = std::env::var("HONEYCOMB_API_KEY") else {
        return Ok(None);
    };
    if api_key.trim().is_empty() {
        return Ok(None);
    }

    let base =
        std::env::var("HONEYCOMB_BASE_URL").unwrap_or_else(|_| HONEYCOMB_DEFAULT_BASE.to_string());
    let endpoint = ensure_traces_path(&base);

    let mut headers = HashMap::with_capacity(1);
    headers.insert("x-honeycomb-team".to_string(), api_key);

    SpanExporter::builder()
        .with_http()
        .with_protocol(Protocol::HttpBinary)
        .with_endpoint(&endpoint)
        .with_headers(headers)
        .with_timeout(EXPORT_TIMEOUT)
        .build()
        .map(Some)
        .map_err(OtelInitError::Exporter)
}

// HTTP Basic over `<public>:<secret>` per Langfuse's OTel ingestion docs;
// `x-langfuse-ingestion-version: 4` opts into the current schema.
fn build_langfuse_exporter() -> Result<Option<SpanExporter>, OtelInitError> {
    let Ok(public) = std::env::var("LANGFUSE_PUBLIC_KEY") else {
        return Ok(None);
    };
    let Ok(secret) = std::env::var("LANGFUSE_SECRET_KEY") else {
        return Ok(None);
    };
    if public.trim().is_empty() || secret.trim().is_empty() {
        return Ok(None);
    }

    let base =
        std::env::var("LANGFUSE_BASE_URL").unwrap_or_else(|_| LANGFUSE_DEFAULT_BASE.to_string());
    let endpoint = ensure_traces_path(&base);

    let token =
        base64::engine::general_purpose::STANDARD.encode(format!("{public}:{secret}").as_bytes());
    let mut headers = HashMap::with_capacity(2);
    headers.insert("Authorization".to_string(), format!("Basic {token}"));
    headers.insert("x-langfuse-ingestion-version".to_string(), "4".to_string());

    SpanExporter::builder()
        .with_http()
        .with_protocol(Protocol::HttpBinary)
        .with_endpoint(&endpoint)
        .with_headers(headers)
        .with_timeout(EXPORT_TIMEOUT)
        .build()
        .map(Some)
        .map_err(OtelInitError::Exporter)
}

/// Wrap a `SpanExporter` in a `BatchSpanProcessor` and then in our event
/// filter. The exporter still pulls from the same batched queue; the filter
/// only mutates `SpanData.events` on the export path.
fn filtered_batch(exporter: SpanExporter) -> EventFilteringProcessor<BatchSpanProcessor> {
    let batch = BatchSpanProcessor::builder(exporter).build();
    EventFilteringProcessor::new(batch, DROPPED_EVENT_NAMES)
}

/// Append `/v1/traces` to a base URL unless it's already there. Trailing
/// slashes on the base are stripped so we never produce `…//v1/traces`.
/// Operators set `*_BASE_URL` to a host or OTLP base; this fills in the
/// trace-signal path the OTel SDK's `with_endpoint` will not.
fn ensure_traces_path(base: &str) -> String {
    let trimmed = base.trim_end_matches('/');
    if trimmed.ends_with(TRACES_PATH) {
        trimmed.to_string()
    } else {
        format!("{trimmed}{TRACES_PATH}")
    }
}

#[derive(Debug, thiserror::Error)]
pub(super) enum OtelInitError {
    #[error("OTLP exporter build: {0}")]
    Exporter(#[source] opentelemetry_otlp::ExporterBuildError),
}

#[cfg(test)]
mod tests {
    use super::ensure_traces_path;

    #[test]
    fn appends_v1_traces_to_bare_host() {
        assert_eq!(
            ensure_traces_path("https://api.honeycomb.io"),
            "https://api.honeycomb.io/v1/traces"
        );
    }

    #[test]
    fn appends_v1_traces_to_otlp_base() {
        assert_eq!(
            ensure_traces_path("https://us.cloud.langfuse.com/api/public/otel"),
            "https://us.cloud.langfuse.com/api/public/otel/v1/traces"
        );
    }

    #[test]
    fn idempotent_when_full_url_supplied() {
        let full = "https://us.cloud.langfuse.com/api/public/otel/v1/traces";
        assert_eq!(ensure_traces_path(full), full);
    }

    #[test]
    fn strips_trailing_slash_before_appending() {
        assert_eq!(
            ensure_traces_path("https://api.honeycomb.io/"),
            "https://api.honeycomb.io/v1/traces"
        );
    }
}
