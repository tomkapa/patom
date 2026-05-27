//! Deterministic in-memory embedding provider for tests.
//!
//! Real embeddings need a network call and a vendor key — neither is
//! appropriate for the unit / integration suite. The fake hashes the
//! input bytes into a fixed-dimension vector. Two identical strings
//! produce identical vectors; different strings produce vectors that
//! differ in at least one bucket. The default dimension matches the
//! production migration (1536) so writes pass the column constraint.

use std::sync::Arc;

use async_trait::async_trait;
use patom_rs::provider::{EmbeddingProvider, ProviderError, SharedEmbeddingProvider};

const DEFAULT_DIM: usize = 1536;

#[derive(Debug, Clone)]
pub struct FakeEmbeddingProvider {
    dim: usize,
}

impl FakeEmbeddingProvider {
    #[must_use]
    pub fn new() -> Self {
        Self { dim: DEFAULT_DIM }
    }

    #[must_use]
    pub fn shared() -> SharedEmbeddingProvider {
        Arc::new(Self::new())
    }
}

impl Default for FakeEmbeddingProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl EmbeddingProvider for FakeEmbeddingProvider {
    fn name(&self) -> &'static str {
        "fake-embedding"
    }

    fn dimensions(&self) -> usize {
        self.dim
    }

    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, ProviderError> {
        Ok(texts.iter().map(|t| hash_to_vec(t, self.dim)).collect())
    }
}

/// Cheap deterministic hash → fixed-dim vector. The mapping is stable
/// across runs and per-byte sensitive enough to make distinct strings
/// produce distinct vectors.
fn hash_to_vec(text: &str, dim: usize) -> Vec<f32> {
    let mut out = vec![0f32; dim];
    for (i, b) in text.bytes().enumerate() {
        let bucket = i % dim;
        out[bucket] += f32::from(b) / 255.0;
    }
    out
}
