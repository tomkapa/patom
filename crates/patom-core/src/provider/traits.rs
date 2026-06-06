use std::fmt;
use std::sync::Arc;

use async_trait::async_trait;

use super::chat::{ChatRequest, ChatResponse};
use super::error::ProviderError;

/// The single seam between the agent and any LLM backend.
///
/// Implementations:
/// * MUST translate provider-specific errors into [`ProviderError`] variants.
/// * MUST NOT panic on malformed responses — return [`ProviderError::Decode`] instead.
/// * SHOULD respect cancellation by returning promptly when the future is dropped (the
///   agent layer wraps every call in `tokio::time::timeout`).
#[async_trait]
pub trait LlmProvider: fmt::Debug + Send + Sync {
    /// Identifier used in tracing fields (`patom.provider`). Low-cardinality, stable.
    fn name(&self) -> &'static str;

    async fn send(&self, request: ChatRequest) -> Result<ChatResponse, ProviderError>;
}

/// Reference-counted handle so the agent can clone cheaply without taking a generic.
pub type SharedProvider = Arc<dyn LlmProvider>;
