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

use crate::provider::{Model, OrgProviderOverlay, ProviderRegistry};

use super::types::AgentRecord;

/// Cheap-clone handle to a [`ModelResolver`].
pub type SharedModelResolver = Arc<dyn ModelResolver>;

/// Strategy for turning `(AgentRecord, ProviderRegistry, OrgProviderOverlay)`
/// into the [`Model`] an agent will use for its next turn.
pub trait ModelResolver: std::fmt::Debug + Send + Sync + 'static {
    /// Resolve the effective model for `record`.
    ///
    /// A provider is *usable* by the agent's org when it is either configured
    /// platform-side (`registry`) **or** the org holds a BYO key for it
    /// (`overlay`) — the union of keyed providers (#141). When the pinned
    /// model's provider is not usable, degrade to the org's default model
    /// (`overlay.default_model`, else the workspace default) rather than
    /// crashing the turn at provider-lookup time.
    fn resolve(
        &self,
        record: &AgentRecord,
        registry: &ProviderRegistry,
        overlay: &OrgProviderOverlay,
    ) -> Model;
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
    fn resolve(
        &self,
        record: &AgentRecord,
        registry: &ProviderRegistry,
        overlay: &OrgProviderOverlay,
    ) -> Model {
        let org = record.org_id;
        // The org's default model overrides the workspace default when set
        // (chosen when the first BYO key is entered, #141).
        let default = overlay.default_model(org).unwrap_or(self.default);
        // Usable = configured platform-side OR the org has a BYO key for it.
        let usable =
            |m: Model| registry.contains(m.provider()) || overlay.has_key(org, m.provider());
        match record.model {
            Some(m) if usable(m) => m,
            Some(m) => {
                // Operational drift: a row pins a model whose provider is
                // neither configured platform-side nor BYO-keyed for this org.
                // Degrade to the org/workspace default (routable by
                // construction) and log loudly so operators can fix it.
                tracing::warn!(
                    event = "agent.model.degraded",
                    patom.agent.id = %record.id,
                    patom.model.pinned = %m,
                    patom.provider.missing = m.provider().as_str(),
                    patom.model.fallback = %default,
                    "pinned model's provider not usable; falling back to default"
                );
                default
            }
            None => default,
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
            allowed_mcp_tools: AllowedMcpTools::empty(),
            model,
            avatar_url: None,
            current_prompt_version_id: crate::agents::PromptVersionId::new(),
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
        let overlay = OrgProviderOverlay::empty();
        let model = resolver.resolve(&record_with(None), &registry, &overlay);
        assert_eq!(model.as_str(), "claude-sonnet-4-5");
    }

    #[test]
    fn returns_pinned_model_when_provider_configured() {
        let default = Model::try_from("claude-sonnet-4-5").expect("catalog");
        let pinned = Model::try_from("gpt-4o-mini").expect("catalog");
        let resolver = StaticAgentModelResolver::new(default);
        let registry = registry_with(&[ProviderId::Anthropic, ProviderId::Openai]);
        let overlay = OrgProviderOverlay::empty();
        let model = resolver.resolve(&record_with(Some(pinned)), &registry, &overlay);
        assert_eq!(model.as_str(), "gpt-4o-mini");
    }

    #[test]
    fn falls_back_to_default_when_pinned_provider_missing() {
        let default = Model::try_from("claude-sonnet-4-6").expect("catalog");
        let pinned = Model::try_from("deepseek-v4-flash").expect("catalog");
        let resolver = StaticAgentModelResolver::new(default);
        let registry = registry_with(&[ProviderId::Anthropic]); // no deepseek
        let overlay = OrgProviderOverlay::empty(); // no BYO keys
        let model = resolver.resolve(&record_with(Some(pinned)), &registry, &overlay);
        assert_eq!(model.as_str(), "claude-sonnet-4-6");
    }

    #[test]
    fn pinned_model_usable_via_byo_key_is_kept() {
        // Provider absent platform-side but the org holds a BYO key for it —
        // the pin is usable (union of keyed providers, #141), no degrade.
        let default = Model::try_from("claude-sonnet-4-6").expect("catalog");
        let pinned = Model::try_from("deepseek-v4-flash").expect("catalog");
        let resolver = StaticAgentModelResolver::new(default);
        let registry = registry_with(&[ProviderId::Anthropic]); // no deepseek platform-side
        let record = record_with(Some(pinned));
        let byo: SharedProvider = Arc::new(StubProvider);
        let overlay =
            OrgProviderOverlay::for_test(vec![(record.org_id, ProviderId::Deepseek, byo)], vec![]);
        let model = resolver.resolve(&record, &registry, &overlay);
        assert_eq!(
            model.as_str(),
            "deepseek-v4-flash",
            "BYO key makes the pin usable"
        );
    }

    #[test]
    fn reroutes_to_per_org_default_when_provider_unusable() {
        // Pinned provider neither platform-configured nor BYO-keyed → degrade,
        // and the org's own default model (not the workspace default) wins.
        let workspace_default = Model::try_from("claude-sonnet-4-6").expect("catalog");
        let org_default = Model::try_from("gpt-5.4-mini").expect("catalog");
        let pinned = Model::try_from("deepseek-v4-flash").expect("catalog");
        let resolver = StaticAgentModelResolver::new(workspace_default);
        let registry = registry_with(&[ProviderId::Anthropic, ProviderId::Openai]); // no deepseek
        let record = record_with(Some(pinned));
        let overlay = OrgProviderOverlay::for_test(vec![], vec![(record.org_id, org_default)]);
        let model = resolver.resolve(&record, &registry, &overlay);
        assert_eq!(
            model.as_str(),
            "gpt-5.4-mini",
            "per-org default overrides workspace"
        );
    }
}
