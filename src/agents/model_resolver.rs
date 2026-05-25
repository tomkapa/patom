//! Per-agent model resolution seam.
//!
//! Sits between the agent record (a row that *may* pin a [`Model`]) and the
//! agent runtime (which needs a concrete [`Model`] to route the chat request).
//! The static resolver shipped today returns `record.model` or the workspace
//! default; future dynamic resolvers (complexity routing, A/B, cost ceiling)
//! drop in by implementing the same trait — no callers of
//! [`crate::agents::AgentFactoryPieces::build`] need to change.
//!
//! The resolver also gets the live [`ProviderRegistry`] so it can fall back
//! gracefully when a row pins a model whose provider has since been removed
//! from `Settings::providers`. That degrades to the default rather than
//! crashing the turn at provider lookup time.

use std::sync::Arc;

use crate::provider::{Model, ProviderRegistry};

use super::types::AgentRecord;

/// Cheap-clone handle to a [`ModelResolver`].
pub type SharedModelResolver = Arc<dyn ModelResolver>;

/// Strategy for turning `(AgentRecord, ProviderRegistry)` into the [`Model`]
/// an agent will use for its next turn.
pub trait ModelResolver: std::fmt::Debug + Send + Sync + 'static {
    /// Resolve the effective model for `record`. `registry` is provided so a
    /// resolver can validate provider availability and degrade gracefully
    /// (e.g. fall back to a known-good default) rather than letting the
    /// runtime crash on a missing provider entry.
    fn resolve(&self, record: &AgentRecord, registry: &ProviderRegistry) -> Model;
}

/// Static resolver: each agent's own `model`, falling back to the workspace default.
///
/// If the agent's pinned model points at a provider that is no longer in the
/// registry, log a warn and return the default — operational drift should
/// degrade visibly, not crash a turn.
#[derive(Debug, Clone, Copy)]
pub struct StaticAgentModelResolver {
    default: Model,
}

impl StaticAgentModelResolver {
    /// Workspace default to use when the agent has no `model` pinned, or
    /// when the pinned provider has been dropped from the registry.
    #[must_use]
    pub const fn new(default: Model) -> Self {
        Self { default }
    }
}

impl ModelResolver for StaticAgentModelResolver {
    fn resolve(&self, record: &AgentRecord, registry: &ProviderRegistry) -> Model {
        match record.model {
            Some(m) if registry.contains(m.provider()) => m,
            Some(m) => {
                // Operational drift: a row pins a model whose provider was
                // dropped from `Settings::providers`. Degrade to the default
                // (whose provider is checked at startup, so it's guaranteed
                // routable) and log loudly so operators can fix it.
                tracing::warn!(
                    event = "agent.model.degraded",
                    relay.agent.id = %record.id,
                    relay.model.pinned = %m,
                    relay.provider.missing = m.provider().as_str(),
                    relay.model.fallback = %self.default,
                    "pinned model's provider not configured; falling back to workspace default"
                );
                self.default
            }
            None => self.default,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::agents::{
        AgentDescription, AgentId, AgentName, AgentRecord, AgentSystemPrompt, AllowedMcpTools,
    };
    use crate::auth::OrgId;
    use crate::provider::{ProviderId, SharedProvider};
    use async_trait::async_trait;
    use chrono::Utc;

    #[derive(Debug)]
    struct StubProvider;
    #[async_trait]
    impl crate::provider::LlmProvider for StubProvider {
        fn name(&self) -> &'static str {
            "stub"
        }
        async fn send(
            &self,
            _req: crate::provider::ChatRequest,
        ) -> Result<crate::provider::ChatResponse, crate::provider::ProviderError> {
            Ok(crate::provider::ChatResponse::default())
        }
    }

    fn record_with(model: Option<Model>) -> AgentRecord {
        let now = Utc::now();
        AgentRecord {
            id: AgentId::new(),
            org_id: OrgId::new(),
            name: AgentName::try_from("r").expect("ok"),
            system_prompt: AgentSystemPrompt::try_from("p").expect("ok"),
            description: AgentDescription::try_from("d").expect("ok"),
            is_default: false,
            allowed_mcp_tools: AllowedMcpTools::empty(),
            model,
            created_at: now,
            updated_at: now,
        }
    }

    fn registry_with(ids: &[ProviderId]) -> ProviderRegistry {
        let mut b = ProviderRegistry::builder();
        for &id in ids {
            let p: SharedProvider = Arc::new(StubProvider);
            b = b.insert(id, p);
        }
        b.build()
    }

    #[test]
    fn returns_default_when_record_has_no_model() {
        let default = Model::try_from("claude-sonnet-4-5").expect("catalog");
        let resolver = StaticAgentModelResolver::new(default);
        let registry = registry_with(&[ProviderId::Anthropic]);
        let model = resolver.resolve(&record_with(None), &registry);
        assert_eq!(model.as_str(), "claude-sonnet-4-5");
    }

    #[test]
    fn returns_pinned_model_when_provider_configured() {
        let default = Model::try_from("claude-sonnet-4-5").expect("catalog");
        let pinned = Model::try_from("gpt-4o-mini").expect("catalog");
        let resolver = StaticAgentModelResolver::new(default);
        let registry = registry_with(&[ProviderId::Anthropic, ProviderId::Openai]);
        let model = resolver.resolve(&record_with(Some(pinned)), &registry);
        assert_eq!(model.as_str(), "gpt-4o-mini");
    }

    #[test]
    fn falls_back_to_default_when_pinned_provider_missing() {
        let default = Model::try_from("claude-sonnet-4-6").expect("catalog");
        let pinned = Model::try_from("deepseek-v4-flash").expect("catalog");
        let resolver = StaticAgentModelResolver::new(default);
        let registry = registry_with(&[ProviderId::Anthropic]); // no deepseek
        let model = resolver.resolve(&record_with(Some(pinned)), &registry);
        assert_eq!(model.as_str(), "claude-sonnet-4-6");
    }
}
