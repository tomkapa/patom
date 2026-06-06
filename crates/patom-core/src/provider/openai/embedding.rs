//! OpenAI-compatible embedding provider.
//!
//! Sibling to [`OpenAiProvider`](super::client::OpenAiProvider). Talks to
//! any endpoint that speaks OpenAI's `/v1/embeddings` shape. Configured
//! independently from the chat side via `EMBEDDING_API_KEY` /
//! `EMBEDDING_BASE_URL` / `EMBEDDING_MODEL` so chat and embeddings can
//! point at different vendors without code change.

use async_openai::Client;
use async_openai::config::OpenAIConfig;
use async_openai::types::embeddings::{CreateEmbeddingRequest, EmbeddingInput};
use async_trait::async_trait;
use tracing::instrument;

use crate::provider::embedding::EmbeddingProvider;
use crate::provider::error::ProviderError;
use crate::types::SecretString;

use super::map_runtime_error;

/// OpenAI-Embeddings implementation of [`EmbeddingProvider`].
pub struct OpenAiEmbeddingProvider {
    client: Client<OpenAIConfig>,
    model: String,
    dimensions: usize,
}

impl std::fmt::Debug for OpenAiEmbeddingProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenAiEmbeddingProvider")
            .field("model", &self.model)
            .field("dimensions", &self.dimensions)
            .finish_non_exhaustive()
    }
}

impl OpenAiEmbeddingProvider {
    /// Construct against an OpenAI-Embeddings-compatible endpoint.
    /// `base_url` lets the operator point at any compatible gateway.
    /// `dimensions` is the column dimension the migration committed to —
    /// `text-embedding-3-small` returns 1536 natively; bumping this
    /// requires a migration.
    pub fn new(
        api_key: &SecretString,
        base_url: Option<String>,
        model: impl Into<String>,
        dimensions: usize,
    ) -> Self {
        // §6: same boundary check the chat provider runs.
        assert!(!api_key.is_empty(), "SecretString invariant: non-empty");
        assert!(
            dimensions > 0,
            "invariant: embedding dimensions must be > 0"
        );

        let mut config = OpenAIConfig::new().with_api_key(api_key.expose());
        if let Some(url) = base_url {
            config = config.with_api_base(url);
        }
        Self {
            client: Client::with_config(config),
            model: model.into(),
            dimensions,
        }
    }
}

#[async_trait]
impl EmbeddingProvider for OpenAiEmbeddingProvider {
    fn name(&self) -> &'static str {
        "openai-embedding"
    }

    fn dimensions(&self) -> usize {
        self.dimensions
    }

    #[instrument(
        name = "provider.openai.embed",
        skip_all,
        fields(
            patom.embedding.provider = "openai-embedding",
            patom.embedding.model = %self.model,
            patom.embedding.batch_size = texts.len(),
        ),
    )]
    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, ProviderError> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }

        let request = CreateEmbeddingRequest {
            model: self.model.clone(),
            input: EmbeddingInput::StringArray(texts.to_vec()),
            encoding_format: None,
            user: None,
            dimensions: None,
        };

        let response = self
            .client
            .embeddings()
            .create(request)
            .await
            .map_err(map_runtime_error)?;

        if response.data.len() != texts.len() {
            return Err(ProviderError::Decode(format!(
                "expected {} embeddings, got {}",
                texts.len(),
                response.data.len()
            )));
        }

        // §6: reject any vector whose dimension does not match the
        // contracted column dimension. Mismatched dims would corrupt the
        // pgvector column on insert.
        let mut out = Vec::with_capacity(response.data.len());
        for (idx, item) in response.data.into_iter().enumerate() {
            if item.embedding.len() != self.dimensions {
                return Err(ProviderError::Decode(format!(
                    "embedding {idx} dim {} does not match expected {}",
                    item.embedding.len(),
                    self.dimensions
                )));
            }
            out.push(item.embedding);
        }
        Ok(out)
    }
}
