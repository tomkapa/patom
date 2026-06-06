//! Span-event filter for OTLP export.
//!
//! Strips span events whose `event_name` attribute matches a configured
//! allow-list before forwarding to the inner `SpanProcessor`. Used to silence
//! the captured-content debug events in `gen_ai.rs` on the OTel pipeline:
//! the payload already rides on the span as `gen_ai.input.messages` /
//! `gen_ai.output.messages`, so re-shipping it as a duplicate span event just
//! inflates Honeycomb cost without adding signal. The events still reach the
//! console layer (CLAUDE.md §2: stderr remains the human-readable channel).
//!
//! A per-layer filter on `OpenTelemetryLayer` would be the more direct knob,
//! but empirically breaks span creation for `#[instrument]` sites in spawned
//! tasks (see `mod.rs`). Filtering at the SpanProcessor seam side-steps that
//! interaction.

use std::time::Duration;

use opentelemetry_sdk::Resource;
use opentelemetry_sdk::error::OTelSdkResult;
use opentelemetry_sdk::trace::{Span as SdkSpan, SpanData, SpanProcessor};

const EVENT_NAME_KEY: &str = "event_name";

/// Wraps an inner `SpanProcessor` and drops span events whose `event_name`
/// attribute matches one of `drop_event_names`. All other events, span
/// attributes, and processor lifecycle calls pass through unchanged.
#[derive(Debug)]
pub(super) struct EventFilteringProcessor<P: SpanProcessor> {
    inner: P,
    drop_event_names: &'static [&'static str],
}

impl<P: SpanProcessor> EventFilteringProcessor<P> {
    pub(super) fn new(inner: P, drop_event_names: &'static [&'static str]) -> Self {
        Self {
            inner,
            drop_event_names,
        }
    }
}

impl<P: SpanProcessor> SpanProcessor for EventFilteringProcessor<P> {
    fn on_start(&self, span: &mut SdkSpan, cx: &opentelemetry::Context) {
        self.inner.on_start(span, cx);
    }

    fn on_end(&self, mut span: SpanData) {
        if !span.events.events.is_empty() {
            let drop_names = self.drop_event_names;
            span.events
                .events
                .retain(|ev| !event_matches(ev, drop_names));
        }
        self.inner.on_end(span);
    }

    fn force_flush(&self) -> OTelSdkResult {
        self.inner.force_flush()
    }

    fn shutdown_with_timeout(&self, timeout: Duration) -> OTelSdkResult {
        self.inner.shutdown_with_timeout(timeout)
    }

    fn set_resource(&mut self, resource: &Resource) {
        self.inner.set_resource(resource);
    }
}

fn event_matches(event: &opentelemetry::trace::Event, drop_names: &[&'static str]) -> bool {
    event.attributes.iter().any(|kv| {
        if kv.key.as_str() != EVENT_NAME_KEY {
            return false;
        }
        let opentelemetry::Value::String(s) = &kv.value else {
            return false;
        };
        drop_names.contains(&s.as_str())
    })
}

#[cfg(test)]
mod tests {
    use std::time::SystemTime;

    use opentelemetry::KeyValue;
    use opentelemetry::trace::Event;

    use super::*;

    fn event_with(event_name: Option<&str>) -> Event {
        let mut attrs = vec![KeyValue::new("level", "DEBUG")];
        if let Some(name) = event_name {
            attrs.push(KeyValue::new(EVENT_NAME_KEY, name.to_string()));
        }
        Event::new("event src/foo.rs:1", SystemTime::now(), attrs, 0)
    }

    #[test]
    fn matches_event_with_listed_name() {
        let event = event_with(Some("foo.bar"));
        assert!(event_matches(&event, &["foo.bar"]));
    }

    #[test]
    fn no_match_when_event_name_absent() {
        let event = event_with(None);
        assert!(!event_matches(&event, &["foo.bar"]));
    }

    #[test]
    fn no_match_when_name_not_in_drop_list() {
        let event = event_with(Some("keep.me"));
        assert!(!event_matches(&event, &["foo.bar"]));
    }

    #[test]
    fn no_match_with_empty_drop_list() {
        let event = event_with(Some("foo.bar"));
        assert!(!event_matches(&event, &[]));
    }
}
