//! OpenAI-Chat-Completions backend for [`LlmProvider`](crate::provider::LlmProvider).
//!
//! Talks to any endpoint that speaks the OpenAI Chat Completions wire format — DeepSeek
//! at `https://api.deepseek.com/v1`, Together, Groq, an in-house gateway, etc. All
//! knowledge of `async_openai` types stays inside this module; the rest of the codebase
//! sees only the provider-agnostic chat types.

mod client;
mod convert;
mod embedding;

use async_openai::error::OpenAIError;

use crate::provider::error::ProviderError;

pub use client::OpenAiProvider;
pub use embedding::OpenAiEmbeddingProvider;

/// Map `async-openai`'s error onto our `ProviderError` variants. Shared by
/// both the chat client and the embedding provider so the discriminant
/// mapping is defined exactly once.
pub(super) fn map_runtime_error(err: OpenAIError) -> ProviderError {
    match err {
        OpenAIError::ApiError(api) => match api.code.as_deref() {
            Some("invalid_api_key" | "authentication_error") => ProviderError::Unauthorized,
            Some("rate_limit_exceeded" | "insufficient_quota") => ProviderError::RateLimited,
            _ => match api.r#type.as_deref() {
                Some("authentication_error") => ProviderError::Unauthorized,
                Some("rate_limit_error") => ProviderError::RateLimited,
                Some("server_error" | "api_error") => ProviderError::Transient(api.to_string()),
                _ => ProviderError::InvalidRequest(api.to_string()),
            },
        },
        OpenAIError::Reqwest(e) if e.is_timeout() || e.is_connect() => {
            ProviderError::Transient(e.to_string())
        }
        OpenAIError::Reqwest(e) => ProviderError::Transport(e.to_string()),
        OpenAIError::JSONDeserialize(e, _) => ProviderError::Decode(e.to_string()),
        OpenAIError::StreamError(e) => ProviderError::Transport(e.to_string()),
        OpenAIError::FileSaveError(s) | OpenAIError::FileReadError(s) => {
            ProviderError::Transport(s)
        }
        OpenAIError::InvalidArgument(s) => ProviderError::InvalidRequest(s),
    }
}
