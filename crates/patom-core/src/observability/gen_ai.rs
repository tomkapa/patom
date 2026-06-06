//! GenAI semantic-convention attributes for chat spans.
//!
//! Records OTel GenAI (`gen_ai.*`) attributes on the current span. The spec is
//! experimental — keys may move between `opentelemetry-semantic-conventions`
//! minor releases, so the constants are isolated here for one-file bumps.
//!
//! Span name is unchanged (CLAUDE.md §2: stable, low-cardinality). Optional
//! input/output capture is gated on `PATOM_GENAI_CAPTURE_CONTENT=1`. The
//! payload is recorded twice: as a `gen_ai.input.messages` /
//! `gen_ai.output.messages` span attribute (queryable on the span itself) and
//! as a `Level::DEBUG` event (visible on stderr for local dev). The duplicate
//! span event the OTel layer would otherwise emit is stripped before export
//! by [`crate::observability::event_filter`] keyed on `event_name`.

use std::sync::OnceLock;

use opentelemetry_semantic_conventions::attribute as semattr;
use tracing::Span;

use crate::provider::{ChatRequest, ChatResponse, StopReason};

/// `event_name` value for the captured-content input debug event.
///
/// The OTel-export pipeline drops span events whose `event_name` attribute
/// matches one of these so the payload is delivered exactly once (as a span
/// attribute), not twice (attribute + duplicate span event).
pub const EVENT_NAME_INPUT: &str = "gen_ai.client.inference.operation.details.input";
/// `event_name` value for the captured-content output debug event. See
/// [`EVENT_NAME_INPUT`] for the duplicate-suppression rationale.
pub const EVENT_NAME_OUTPUT: &str = "gen_ai.client.inference.operation.details.output";

/// Span-attribute keys for captured input/output payloads. Not yet in
/// `opentelemetry-semantic-conventions` 0.31; spec is experimental.
const ATTR_INPUT_MESSAGES: &str = "gen_ai.input.messages";
const ATTR_OUTPUT_MESSAGES: &str = "gen_ai.output.messages";

fn capture_content() -> bool {
    static FLAG: OnceLock<bool> = OnceLock::new();
    *FLAG.get_or_init(|| {
        matches!(
            std::env::var("PATOM_GENAI_CAPTURE_CONTENT").as_deref(),
            Ok("1" | "true" | "TRUE" | "yes")
        )
    })
}

/// Record request-shape attributes on the current span. Call inside the
/// `#[instrument]` body, before the network call. `system_name` is the GenAI
/// provider id (`"anthropic"`, `"openai"`).
pub fn record_chat_request(system_name: &'static str, req: &ChatRequest) {
    let span = Span::current();
    span.record(semattr::GEN_AI_SYSTEM, system_name);
    span.record(semattr::GEN_AI_OPERATION_NAME, "chat");
    span.record(semattr::GEN_AI_REQUEST_MODEL, req.model.as_str());
    span.record(
        semattr::GEN_AI_REQUEST_MAX_TOKENS,
        i64::from(req.max_output_tokens.get()),
    );

    if capture_content() {
        let messages = serde_json::json!({
            "system": req.system.as_ref(),
            "messages": req.messages,
        });
        if let Ok(payload) = serde_json::to_string(&messages) {
            span.record(ATTR_INPUT_MESSAGES, payload.as_str());
            tracing::debug!(
                event_name = EVENT_NAME_INPUT,
                gen_ai.input.messages = %payload,
            );
        }
    }
}

/// Record response-shape attributes on the current span. Call after the
/// provider returns, before the function exits.
pub fn record_chat_response(resp: &ChatResponse) {
    let span = Span::current();
    if let Some(model) = resp.model.as_ref() {
        span.record(semattr::GEN_AI_RESPONSE_MODEL, model.as_str());
    }
    span.record(
        semattr::GEN_AI_RESPONSE_FINISH_REASONS,
        finish_reason_str(&resp.stop_reason),
    );
    span.record(
        semattr::GEN_AI_USAGE_INPUT_TOKENS,
        i64::from(resp.usage.input_tokens),
    );
    span.record(
        semattr::GEN_AI_USAGE_OUTPUT_TOKENS,
        i64::from(resp.usage.output_tokens),
    );
    if let Some(c) = resp.usage.cache_creation_input_tokens {
        span.record("gen_ai.usage.cache_creation_input_tokens", i64::from(c));
    }
    if let Some(c) = resp.usage.cache_read_input_tokens {
        span.record("gen_ai.usage.cache_read_input_tokens", i64::from(c));
    }

    if capture_content() {
        let body = serde_json::json!({ "content": resp.content });
        if let Ok(payload) = serde_json::to_string(&body) {
            span.record(ATTR_OUTPUT_MESSAGES, payload.as_str());
            tracing::debug!(
                event_name = EVENT_NAME_OUTPUT,
                gen_ai.output.messages = %payload,
            );
        }
    }
}

fn finish_reason_str(reason: &StopReason) -> &'static str {
    match reason {
        StopReason::EndTurn => "stop",
        StopReason::ToolUse => "tool_calls",
        StopReason::MaxTokens => "length",
        StopReason::Other(_) => "other",
    }
}
