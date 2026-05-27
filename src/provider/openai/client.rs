use async_openai::Client;
use async_openai::config::OpenAIConfig;
use async_trait::async_trait;
use tracing::instrument;

use super::convert::{
    ChatRequestBody, ChatResponseBody, choice_to_content, map_finish_reason, message_to_wire,
    system_message, tool_spec_to_wire,
};
use super::map_runtime_error;
use crate::observability::gen_ai;
use crate::provider::chat::{ChatRequest, ChatResponse, Usage};
use crate::provider::error::ProviderError;
use crate::provider::traits::LlmProvider;
use crate::types::{ModelId, SecretString};

/// OpenAI-Chat-Completions implementation of [`LlmProvider`].
///
/// The `backend` field carries the [`ProviderId`] this instance was
/// constructed for (`Openai`, `Deepseek`, …). Two instances can point at the
/// same SDK with different base URLs and API keys — the id is what surfaces
/// in tracing (`patom.provider`) so analytics can distinguish them. Typing
/// it as `ProviderId` instead of `&'static str` enforces the low-cardinality
/// label invariant at the type level (CLAUDE.md §2).
pub struct OpenAiProvider {
    backend: crate::provider::ProviderId,
    client: Client<OpenAIConfig>,
}

impl std::fmt::Debug for OpenAiProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenAiProvider")
            .field("backend", &self.backend)
            .finish_non_exhaustive()
    }
}

impl OpenAiProvider {
    /// Construct a provider against an OpenAI-Chat-Completions-compatible endpoint.
    /// Pass `base_url` to point at DeepSeek (`https://api.deepseek.com/v1`), Together,
    /// Groq, or any in-house gateway. Omit it to hit the public OpenAI API.
    /// `backend` is the low-cardinality [`ProviderId`] used in tracing fields.
    pub fn new(
        backend: crate::provider::ProviderId,
        api_key: &SecretString,
        base_url: Option<String>,
    ) -> Self {
        // §6: assert the boundary precondition the type system already proves, so a
        // future refactor that loosens `SecretString` does not silently let an empty key
        // through.
        assert!(!api_key.is_empty(), "SecretString invariant: non-empty");

        let mut config = OpenAIConfig::new().with_api_key(api_key.expose());
        if let Some(url) = base_url {
            config = config.with_api_base(url);
        }
        Self {
            backend,
            client: Client::with_config(config),
        }
    }

    /// Sugar for the public OpenAI backend.
    #[must_use]
    pub fn openai(api_key: &SecretString, base_url: Option<String>) -> Self {
        Self::new(crate::provider::ProviderId::Openai, api_key, base_url)
    }

    /// Sugar for DeepSeek. Defaults `base_url` to the public DeepSeek host
    /// when the operator did not override it.
    #[must_use]
    pub fn deepseek(api_key: &SecretString, base_url: Option<String>) -> Self {
        let base = base_url.or_else(|| Some("https://api.deepseek.com/v1".to_owned()));
        Self::new(crate::provider::ProviderId::Deepseek, api_key, base)
    }
}

#[async_trait]
impl LlmProvider for OpenAiProvider {
    fn name(&self) -> &'static str {
        self.backend.as_str()
    }

    // GenAI semconv fields are declared `Empty` here and recorded inside the body so
    // both the request- and response-shaped attributes ride on the same span. Keeping
    // the span name stable (`provider.openai.send`) per CLAUDE.md §2; the spec's
    // recommended `chat <model>` form would put the model in the name and inflate
    // cardinality. The `patom.provider` field is populated dynamically from
    // `self.backend` so DeepSeek (and any future OpenAI-SDK-compatible backend)
    // reports its own backend label without forking the implementation.
    #[instrument(
        name = "provider.openai.send",
        skip_all,
        fields(
            patom.provider = self.backend.as_str(),
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
            gen_ai.input.messages = tracing::field::Empty,
            gen_ai.output.messages = tracing::field::Empty,
        ),
    )]
    async fn send(&self, request: ChatRequest) -> Result<ChatResponse, ProviderError> {
        gen_ai::record_chat_request(self.backend.as_str(), &request);

        let mut messages = Vec::with_capacity(request.messages.len() + 1);
        messages.push(system_message(&request.system));
        for msg in request.messages {
            messages.extend(message_to_wire(msg));
        }

        // `.then(|| …)` defers the `.collect()` to the non-empty case so we don't
        // allocate an empty `Vec` only to throw it away.
        let tools = (!request.tools.is_empty())
            .then(|| request.tools.iter().map(tool_spec_to_wire).collect());

        let body = ChatRequestBody {
            model: request.model.as_str().to_string(),
            messages,
            tools,
            max_completion_tokens: Some(request.max_output_tokens.get()),
        };

        // Use the BYOT (bring-your-own-type) path so we can send `reasoning_content` on
        // assistant messages and read it back on responses — fields the stock
        // async-openai schema doesn't model. See `convert.rs` for why this matters.
        let response: ChatResponseBody = self
            .client
            .chat()
            .create_byot(body)
            .await
            .map_err(map_runtime_error)?;

        // Extract usage / model before consuming `response.choices` below — these
        // attributes ride on the span, not on the request shape.
        let usage = response
            .usage
            .as_ref()
            .map(|u| Usage {
                input_tokens: u.prompt_tokens,
                output_tokens: u.completion_tokens,
                cache_creation_input_tokens: u.cache_creation_tokens(),
                cache_read_input_tokens: u.cache_read_tokens(),
            })
            .unwrap_or_default();
        let model = response
            .model
            .as_deref()
            .and_then(|m| ModelId::try_from(m).ok());

        let choice = response
            .choices
            .into_iter()
            .next()
            .ok_or(ProviderError::EmptyResponse)?;
        let stop_reason = map_finish_reason(choice.finish_reason);
        let content = choice_to_content(choice);
        if content.is_empty() {
            return Err(ProviderError::EmptyResponse);
        }

        let resp = ChatResponse {
            content,
            stop_reason,
            usage,
            model,
        };
        gen_ai::record_chat_response(&resp);
        Ok(resp)
    }
}
