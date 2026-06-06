//! Embedding provider seam.
//!
//! Sibling to [`LlmProvider`](super::traits::LlmProvider): same SDK family,
//! a different surface, separately configured. Memory's `recall` tool, the
//! contextual-layer retrieval, and the librarian's dedup/contradiction
//! sweep all read this trait — never a concrete impl.
//!
//! Failure handling is the same as the chat side: errors map to
//! [`ProviderError`] variants. Retrieval-degraded paths are the only
//! callers expected to swallow failures.

use std::fmt;
use std::sync::Arc;

use async_trait::async_trait;

use super::error::ProviderError;

/// Embeds text into vectors. Implementations MUST:
///
/// * Return one vector per input string in the same order.
/// * Produce vectors of length [`Self::dimensions`] for every input.
/// * Translate provider-specific errors into [`ProviderError`] variants.
#[async_trait]
pub trait EmbeddingProvider: fmt::Debug + Send + Sync {
    /// Stable, low-cardinality identifier for tracing
    /// (`patom.embedding.provider`). Same shape as
    /// [`LlmProvider::name`](super::traits::LlmProvider::name).
    fn name(&self) -> &'static str;

    /// Embedding vector dimensionality. The migration constrains the
    /// `agent_memories.embedding` column to `vector(1536)` today; the
    /// composition root verifies the provider matches at startup.
    fn dimensions(&self) -> usize;

    /// Embed `texts` as a single batch. Implementations SHOULD batch into
    /// one provider call rather than one per element.
    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, ProviderError>;
}

/// Reference-counted handle so collaborators can hold the provider
/// without taking a generic.
pub type SharedEmbeddingProvider = Arc<dyn EmbeddingProvider>;

/// Embed a single piece of text. Convenience wrapper around
/// [`EmbeddingProvider::embed`]; returns the only vector or fails.
pub async fn embed_one(
    provider: &dyn EmbeddingProvider,
    text: &str,
) -> Result<Vec<f32>, ProviderError> {
    let mut out = provider.embed(&[text.to_owned()]).await?;
    if out.len() != 1 {
        return Err(ProviderError::Decode(format!(
            "embedding provider returned {} vectors for 1 input",
            out.len()
        )));
    }
    Ok(out.remove(0))
}
