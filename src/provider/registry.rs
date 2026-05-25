//! Process-wide registry of [`SharedProvider`]s keyed by [`ProviderId`].
//!
//! Built once at startup from the configured provider credentials and never
//! mutated thereafter. The collection is bounded by the closed [`ProviderId`]
//! enum (CLAUDE.md §5), so no growth-on-demand concern.
//!
//! Lookup is O(1) via [`std::collections::HashMap`]. The registry is the
//! routing surface that `Agent::call_provider` consults per turn — given a
//! [`crate::provider::Model`], the agent calls `registry.get(model.provider())`
//! to pick the right backend. The invariant that backs the `Option::expect`
//! at the call site is upheld by two layers (CLAUDE.md §6):
//! 1. startup validation
//!    ([`crate::config::SettingsError::DefaultModelProviderNotConfigured`])
//!    guarantees the workspace default's provider is present, and
//! 2. [`crate::agents::StaticAgentModelResolver`] degrades any per-agent
//!    pin whose provider has been dropped from config back to that default
//!    (with a tracing warn).

use std::collections::HashMap;
use std::sync::Arc;

use super::id::ProviderId;
use super::traits::SharedProvider;

/// Cheap-clone handle to a [`ProviderRegistry`].
pub type SharedProviderRegistry = Arc<ProviderRegistry>;

/// Immutable map from [`ProviderId`] to its built provider instance.
#[derive(Debug)]
pub struct ProviderRegistry {
    providers: HashMap<ProviderId, SharedProvider>,
}

impl ProviderRegistry {
    /// New empty builder. Use [`ProviderRegistryBuilder::insert`] to add
    /// each configured backend.
    #[must_use]
    pub fn builder() -> ProviderRegistryBuilder {
        ProviderRegistryBuilder::default()
    }

    /// Provider instance for `id`, if configured. `None` means the operator
    /// did not supply credentials for this backend; routing to it is a
    /// startup-validated invariant violation.
    #[must_use]
    pub fn get(&self, id: ProviderId) -> Option<&SharedProvider> {
        self.providers.get(&id)
    }

    /// Whether `id` has a configured provider in this registry.
    #[must_use]
    pub fn contains(&self, id: ProviderId) -> bool {
        self.providers.contains_key(&id)
    }

    /// Iterate every configured [`ProviderId`]. Bounded by the closed enum.
    pub fn configured(&self) -> impl Iterator<Item = ProviderId> + '_ {
        self.providers.keys().copied()
    }
}

/// Builder so the composition root can insert one provider per configured
/// backend, then freeze with [`ProviderRegistryBuilder::build`].
#[derive(Debug, Default)]
pub struct ProviderRegistryBuilder {
    providers: HashMap<ProviderId, SharedProvider>,
}

impl ProviderRegistryBuilder {
    /// Insert (or overwrite) the provider for `id`.
    #[must_use]
    pub fn insert(mut self, id: ProviderId, provider: SharedProvider) -> Self {
        self.providers.insert(id, provider);
        self
    }

    /// Freeze the builder into an immutable [`ProviderRegistry`].
    #[must_use]
    pub fn build(self) -> ProviderRegistry {
        ProviderRegistry {
            providers: self.providers,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::provider::ChatResponse;
    use crate::provider::chat::ChatRequest;
    use crate::provider::error::ProviderError;
    use crate::provider::traits::LlmProvider;
    use async_trait::async_trait;

    #[derive(Debug)]
    struct StubProvider(&'static str);

    #[async_trait]
    impl LlmProvider for StubProvider {
        fn name(&self) -> &'static str {
            self.0
        }
        async fn send(&self, _req: ChatRequest) -> Result<ChatResponse, ProviderError> {
            Ok(ChatResponse::default())
        }
    }

    fn stub(name: &'static str) -> SharedProvider {
        Arc::new(StubProvider(name))
    }

    #[test]
    fn missing_provider_returns_none() {
        let registry = ProviderRegistry::builder().build();
        assert!(registry.get(ProviderId::Anthropic).is_none());
        assert!(!registry.contains(ProviderId::Anthropic));
    }

    #[test]
    fn inserted_provider_is_retrievable() {
        let registry = ProviderRegistry::builder()
            .insert(ProviderId::Openai, stub("openai"))
            .insert(ProviderId::Deepseek, stub("deepseek"))
            .build();
        assert!(registry.contains(ProviderId::Openai));
        assert!(registry.contains(ProviderId::Deepseek));
        assert!(!registry.contains(ProviderId::Anthropic));

        let configured: std::collections::HashSet<_> = registry.configured().collect();
        assert_eq!(configured.len(), 2);
        assert!(configured.contains(&ProviderId::Openai));
        assert!(configured.contains(&ProviderId::Deepseek));
    }
}
