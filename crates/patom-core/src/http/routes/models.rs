//! Per-org catalog of LLM models the workspace can pin per-agent.
//!
//! `GET /models` — returns the catalog entries the **caller's org** can
//! actually route, i.e. every [`crate::provider::catalog::MODEL_CATALOG`] model
//! whose provider is *usable*: configured platform-side (the operator's keys —
//! DeepSeek only in the default cloud build) **or** held as a BYO key for the
//! org (#141). This is the same union-of-keyed-providers predicate the model
//! resolver routes on ([`crate::agents::StaticAgentModelResolver`]), so the
//! agent-detail picker can only ever offer a model the next turn can route.
//!
//! Concretely: an org with no BYO key sees only the platform providers'
//! models (DeepSeek on cloud); adding a BYO Anthropic key surfaces Anthropic's
//! models alongside them; adding a BYO DeepSeek key is a no-op when DeepSeek is
//! already platform-configured. Authenticated read, org-scoped.

use axum::Json;
use axum::Router;
use axum::extract::State;
use axum::routing::get;
use serde::Serialize;

use crate::auth::{OrgId, Principal};
use crate::provider::{Model, OrgProviderOverlay, ProviderRegistry};

use super::super::state::AppState;

pub(super) fn router() -> Router<AppState> {
    Router::new().route("/models", get(list_models))
}

/// One catalog row on the wire. Mirrors
/// [`crate::provider::catalog::CatalogEntry`] but flattens the provider
/// to its `snake_case` discriminator so the FE can render a chip without
/// holding a provider enum of its own.
#[derive(Debug, Serialize)]
struct ModelEntry {
    /// Catalog id the agent's `model` field accepts on PUT, e.g.
    /// `"claude-sonnet-4-6"`. The FE sends this verbatim.
    id: &'static str,
    /// `snake_case` provider name (`"anthropic"`, `"openai"`, …) for
    /// rendering a per-row chip on the picker.
    provider: &'static str,
}

/// Catalog models `org` can route, given the platform `registry` and the BYO
/// `overlay`. Pure (no I/O) so the filtering rule is unit-tested directly. A
/// provider is usable when it is platform-configured **or** the org holds a BYO
/// key for it — identical to the resolver's routability check, so the picker
/// never offers an unroutable model nor hides a routable one.
fn usable_models(
    registry: &ProviderRegistry,
    overlay: &OrgProviderOverlay,
    org: OrgId,
) -> Vec<ModelEntry> {
    Model::all()
        .filter(|m| registry.contains(m.provider()) || overlay.has_key(org, m.provider()))
        .map(|m| ModelEntry {
            id: m.as_str(),
            provider: m.provider().as_str(),
        })
        .collect()
}

#[tracing::instrument(name = "models.list", skip_all, fields(patom.org.id = %principal.active_org_id))]
async fn list_models(State(state): State<AppState>, principal: Principal) -> Json<Vec<ModelEntry>> {
    let out = usable_models(
        &state.providers,
        &state.provider_overlay,
        principal.active_org_id,
    );
    Json(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{ProviderId, SharedProvider};
    use std::sync::Arc;

    #[derive(Debug)]
    struct StubProvider;
    #[async_trait::async_trait]
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

    fn registry_with(ids: &[ProviderId]) -> ProviderRegistry {
        let mut b = ProviderRegistry::builder();
        for &id in ids {
            let p: SharedProvider = Arc::new(StubProvider);
            b = b.insert(id, p);
        }
        b.build()
    }

    fn providers_of(entries: &[ModelEntry]) -> std::collections::HashSet<&'static str> {
        entries.iter().map(|e| e.provider).collect()
    }

    #[test]
    fn only_platform_deepseek_when_no_byo_key() {
        // Default cloud build: the operator configured DeepSeek only and the
        // org holds no BYO key. The picker must show DeepSeek models and
        // nothing else.
        let registry = registry_with(&[ProviderId::Deepseek]);
        let overlay = OrgProviderOverlay::empty();
        let out = usable_models(&registry, &overlay, OrgId::new());

        assert!(!out.is_empty(), "deepseek models must surface");
        assert_eq!(providers_of(&out), std::iter::once("deepseek").collect());
        assert!(out.iter().any(|e| e.id == "deepseek-v4-flash"));
    }

    #[test]
    fn byo_anthropic_key_adds_anthropic_alongside_platform() {
        // Org brings an Anthropic key on top of the platform DeepSeek. Both
        // vendors' models route now, so both appear; OpenAI (neither
        // platform-configured nor BYO) stays hidden.
        let org = OrgId::new();
        let registry = registry_with(&[ProviderId::Deepseek]);
        let byo: SharedProvider = Arc::new(StubProvider);
        let overlay = OrgProviderOverlay::for_test(vec![(org, ProviderId::Anthropic, byo)], vec![]);
        let out = usable_models(&registry, &overlay, org);

        assert_eq!(
            providers_of(&out),
            ["deepseek", "anthropic"].into_iter().collect(),
            "union of platform + BYO, no openai"
        );
    }

    #[test]
    fn byo_key_scoped_to_its_own_org() {
        // A BYO Anthropic key held by another org must not widen this org's
        // picker — the predicate keys on the caller's org id.
        let mine = OrgId::new();
        let other = OrgId::new();
        let registry = registry_with(&[ProviderId::Deepseek]);
        let byo: SharedProvider = Arc::new(StubProvider);
        let overlay =
            OrgProviderOverlay::for_test(vec![(other, ProviderId::Anthropic, byo)], vec![]);
        let out = usable_models(&registry, &overlay, mine);

        assert_eq!(
            providers_of(&out),
            std::iter::once("deepseek").collect(),
            "another org's BYO key must not leak into mine"
        );
    }

    #[test]
    fn empty_when_no_usable_provider() {
        // No platform key, no BYO key → nothing routable → empty picker
        // (the FE renders the inherit-default option regardless).
        let registry = registry_with(&[]);
        let overlay = OrgProviderOverlay::empty();
        let out = usable_models(&registry, &overlay, OrgId::new());
        assert!(out.is_empty());
    }
}
