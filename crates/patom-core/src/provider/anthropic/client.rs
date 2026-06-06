use async_trait::async_trait;
use claudius::{Anthropic, MessageCreateParams};
use tracing::instrument;

use super::convert::{
    block_to_assistant, map_stop_reason, message_to_param, parse_model, tool_spec_to_param,
};
use crate::observability::gen_ai;
use crate::provider::chat::{ChatRequest, ChatResponse, Usage};
use crate::provider::error::ProviderError;
use crate::provider::traits::LlmProvider;
use crate::types::{ModelId, SecretString};

/// Anthropic-Messages-API implementation of [`LlmProvider`].
pub struct AnthropicProvider {
    client: Anthropic,
}

impl std::fmt::Debug for AnthropicProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AnthropicProvider").finish_non_exhaustive()
    }
}

impl AnthropicProvider {
    /// Construct a provider against the public API. Pass `base_url` to point at a proxy
    /// or compatible endpoint.
    pub fn new(api_key: &SecretString, base_url: Option<String>) -> Result<Self, ProviderError> {
        // §6: assert the boundary precondition the type system already proves, so a future
        // refactor that loosens `SecretString` does not silently let an empty key through.
        assert!(!api_key.is_empty(), "SecretString invariant: non-empty");

        let mut client =
            Anthropic::new(Some(api_key.expose().to_string())).map_err(map_construction_error)?;
        if let Some(url) = base_url {
            client = client.with_base_url(url);
        }
        Ok(Self { client })
    }
}

#[async_trait]
impl LlmProvider for AnthropicProvider {
    fn name(&self) -> &'static str {
        "anthropic"
    }

    // GenAI semconv fields are declared `Empty` here and recorded inside the body so
    // both the request- and response-shaped attributes ride on the same span. Keeping
    // the span name stable (`provider.anthropic.send`) per CLAUDE.md §2.
    #[instrument(
        name = "provider.anthropic.send",
        skip_all,
        fields(
            patom.provider = "anthropic",
            patom.model = %request.model,
            patom.messages = request.messages.len(),
            patom.tools = request.tools.len(),
            patom.max_output_tokens = request.max_output_tokens.get(),
            gen_ai.system = tracing::field::Empty,
            gen_ai.operation.name = tracing::field::Empty,
            gen_ai.request.model = tracing::field::Empty,
            gen_ai.request.max_tokens = tracing::field::Empty,
            gen_ai.response.model = tracing::field::Empty,
            gen_ai.response.finish_reasons = tracing::field::Empty,
            gen_ai.usage.input_tokens = tracing::field::Empty,
            gen_ai.usage.output_tokens = tracing::field::Empty,
            gen_ai.usage.cache_creation_input_tokens = tracing::field::Empty,
            gen_ai.usage.cache_read_input_tokens = tracing::field::Empty,
            gen_ai.input.messages = tracing::field::Empty,
            gen_ai.output.messages = tracing::field::Empty,
        ),
    )]
    async fn send(&self, request: ChatRequest) -> Result<ChatResponse, ProviderError> {
        gen_ai::record_chat_request("anthropic", &request);

        let model = parse_model(request.model.as_str());

        // `.then(|| …)` defers the `.collect()` to the non-empty case so we don't
        // allocate an empty `Vec` only to throw it away.
        let tools = (!request.tools.is_empty())
            .then(|| request.tools.iter().map(tool_spec_to_param).collect());

        let messages = request
            .messages
            .into_iter()
            .filter_map(message_to_param)
            .collect();

        let mut params = MessageCreateParams::new(request.max_output_tokens.get(), messages, model)
            .with_system_string(request.system.to_string());
        if let Some(tools) = tools {
            params = params.with_tools(tools);
        }

        let response = self.client.send(params).await.map_err(map_runtime_error)?;

        // Pull usage + response model off the wire response before we consume
        // `response.content` below. Anthropic always returns `usage`; the cache
        // counters are `Option<i32>` and may be absent on non-cached calls.
        let usage = Usage {
            input_tokens: clamp_u32(response.usage.input_tokens),
            output_tokens: clamp_u32(response.usage.output_tokens),
            cache_creation_input_tokens: response.usage.cache_creation_input_tokens.map(clamp_u32),
            cache_read_input_tokens: response.usage.cache_read_input_tokens.map(clamp_u32),
        };
        let response_model = ModelId::try_from(response.model.to_string().as_str()).ok();

        let content: Vec<_> = response
            .content
            .into_iter()
            .filter_map(block_to_assistant)
            .collect();
        if content.is_empty() {
            return Err(ProviderError::EmptyResponse);
        }

        let resp = ChatResponse {
            content,
            stop_reason: map_stop_reason(response.stop_reason),
            usage,
            model: response_model,
        };
        gen_ai::record_chat_response(&resp);
        Ok(resp)
    }
}

/// Anthropic reports token counters as signed `i32` (the SDK mirrors the wire shape).
/// Negatives are not legal — the counters are non-negative by definition — so a sign
/// flip is a programmer bug worth a saturating clamp rather than a parse error. CLAUDE.md
/// §1 forbids `as` narrowing; we go through `try_into` and saturate on the impossible
/// arm.
fn clamp_u32(n: i32) -> u32 {
    u32::try_from(n).unwrap_or(0)
}

/// Errors raised during client construction (bad URL, missing key etc.).
fn map_construction_error(err: claudius::Error) -> ProviderError {
    if err.is_authentication() {
        return ProviderError::Unauthorized;
    }
    ProviderError::InvalidRequest(err.to_string())
}

/// Errors raised by `send` — surface the most actionable variant. Callers (the agent
/// retry/timeout layer) only branch on the `ProviderError` discriminant, never the
/// underlying string.
fn map_runtime_error(err: claudius::Error) -> ProviderError {
    if err.is_authentication() {
        return ProviderError::Unauthorized;
    }
    if err.is_rate_limit() {
        return ProviderError::RateLimited;
    }
    if err.is_timeout() || err.is_connection() || err.is_server_error() {
        return ProviderError::Transient(err.to_string());
    }
    if err.is_bad_request() {
        return ProviderError::InvalidRequest(err.to_string());
    }
    ProviderError::Transport(err.to_string())
}
