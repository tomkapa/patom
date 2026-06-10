//! Per-org BYO provider overlay (#141).
//!
//! A process-wide, refreshed cache of each org's decrypted BYO provider clients
//! plus its default model. The agent factory consults it **synchronously** on
//! the per-turn hot path — `get(org, provider)` returns a cheap `Arc` clone of
//! a ready provider client, or `None` to fall back to the platform registry.
//!
//! Mirrors [`crate::mcp::McpRegistry`]: a cheap-clone handle around an
//! `Arc<Inner>` whose state is swapped wholesale on each refresh, so steady-
//! state reads are a single read-lock + `Arc` clone with no DB round-trip. A
//! background [`super::refresher::ProviderRefresher`] runs `refresh` whenever a
//! credential CRUD mutation signals it, so a newly-saved key activates within
//! one tick — independent of the agent cache TTL.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use tracing::{instrument, warn};

use crate::auth::OrgId;

use super::anthropic::AnthropicProvider;
use super::catalog::Model;
use super::credentials::{
    ProviderApiKey, ProviderBaseUrl, ProviderCredentialError, SharedOrgProviderCredentialStore,
};
use super::error::ProviderError;
use super::id::ProviderId;
use super::limits::MAX_ORGS_OVERLAY;
use super::openai::OpenAiProvider;
use super::traits::SharedProvider;

/// Construct a ready provider client from a decrypted BYO key (+ optional base
/// URL). Shared by the overlay refresher and the HTTP validate-key handler so
/// the BYO client is built exactly one way.
pub fn build_byo_client(
    provider: ProviderId,
    api_key: &ProviderApiKey,
    base_url: Option<&ProviderBaseUrl>,
) -> Result<SharedProvider, ProviderError> {
    let secret = api_key.as_secret();
    let url = base_url.map(|u| u.as_str().to_owned());
    let client: SharedProvider = match provider {
        ProviderId::Anthropic => Arc::new(AnthropicProvider::new(secret, url)?),
        ProviderId::Openai => Arc::new(OpenAiProvider::openai(secret, url)),
        ProviderId::Deepseek => Arc::new(OpenAiProvider::deepseek(secret, url)),
    };
    Ok(client)
}

/// One org's BYO routing state: the providers it holds a usable key for, plus
/// its default model (if any).
#[derive(Default)]
struct OrgEntry {
    providers: HashMap<ProviderId, SharedProvider>,
    default_model: Option<Model>,
}

#[derive(Default)]
struct OverlayState {
    orgs: HashMap<OrgId, OrgEntry>,
}

struct OverlayInner {
    state: RwLock<OverlayState>,
    /// `None` for an [`empty`](OrgProviderOverlay::empty) overlay — the
    /// platform-only routing used by unit tests and the standalone
    /// `build_agent` path, where `refresh` is a no-op and every lookup misses.
    store: Option<SharedOrgProviderCredentialStore>,
    org_cap: usize,
}

impl std::fmt::Debug for OverlayInner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let guard = self.state.read().expect("overlay lock poisoned");
        f.debug_struct("OrgProviderOverlay")
            .field("orgs", &guard.orgs.len())
            .field("org_cap", &self.org_cap)
            .finish_non_exhaustive()
    }
}

/// Cheap-clone handle to the per-org BYO provider overlay.
#[derive(Clone, Debug)]
pub struct OrgProviderOverlay(Arc<OverlayInner>);

impl OrgProviderOverlay {
    /// Build an overlay backed by the credential store. Call
    /// [`refresh`](Self::refresh) once at startup before the worker pool
    /// begins, so the first turn already sees BYO keys.
    #[must_use]
    pub fn new(store: SharedOrgProviderCredentialStore) -> Self {
        Self(Arc::new(OverlayInner {
            state: RwLock::new(OverlayState::default()),
            store: Some(store),
            org_cap: MAX_ORGS_OVERLAY,
        }))
    }

    /// Build a permanently-empty overlay (no store). Every lookup misses and
    /// `refresh` is a no-op — i.e. platform-only routing. Used by unit tests
    /// and the standalone [`crate::app::build_agent`] path that has no BYO
    /// infrastructure wired.
    #[must_use]
    pub fn empty() -> Self {
        Self(Arc::new(OverlayInner {
            state: RwLock::new(OverlayState::default()),
            store: None,
            org_cap: MAX_ORGS_OVERLAY,
        }))
    }

    /// Install a synthetic overlay state for tests — `(org, provider, client)`
    /// client entries plus `(org, default_model)` pairs — bypassing the store
    /// and `refresh`. Gated on the `test-catalog` feature (auto-enabled for the
    /// crate's own tests and for integration tests via dev-dependency) so it
    /// never reaches a release artifact.
    #[cfg(feature = "test-catalog")]
    #[must_use]
    pub fn for_test(
        clients: Vec<(OrgId, ProviderId, SharedProvider)>,
        defaults: Vec<(OrgId, Model)>,
    ) -> Self {
        let mut orgs: HashMap<OrgId, OrgEntry> = HashMap::new();
        for (org, model) in defaults {
            orgs.entry(org).or_default().default_model = Some(model);
        }
        for (org, provider, client) in clients {
            orgs.entry(org)
                .or_default()
                .providers
                .insert(provider, client);
        }
        Self(Arc::new(OverlayInner {
            state: RwLock::new(OverlayState { orgs }),
            store: None,
            org_cap: MAX_ORGS_OVERLAY,
        }))
    }

    /// The BYO client `org` holds for `provider`, if any. A cheap `Arc` clone;
    /// `None` means "no usable BYO key — use the platform client".
    #[must_use]
    pub fn get(&self, org: OrgId, provider: ProviderId) -> Option<SharedProvider> {
        let guard = self.0.state.read().expect("overlay lock poisoned");
        guard.orgs.get(&org)?.providers.get(&provider).cloned()
    }

    /// Whether `org` holds a usable BYO key for `provider`. The routing
    /// predicate the resolver uses to compute the union of usable providers.
    #[must_use]
    pub fn has_key(&self, org: OrgId, provider: ProviderId) -> bool {
        let guard = self.0.state.read().expect("overlay lock poisoned");
        guard
            .orgs
            .get(&org)
            .is_some_and(|e| e.providers.contains_key(&provider))
    }

    /// The org's default model, if set. `None` = fall back to the workspace
    /// default (`Settings::model`).
    #[must_use]
    pub fn default_model(&self, org: OrgId) -> Option<Model> {
        let guard = self.0.state.read().expect("overlay lock poisoned");
        guard.orgs.get(&org).and_then(|e| e.default_model)
    }

    /// Number of orgs currently held in the overlay (test/metrics aid).
    #[must_use]
    pub fn len(&self) -> usize {
        self.0
            .state
            .read()
            .expect("overlay lock poisoned")
            .orgs
            .len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Re-read every BYO credential + per-org default model, rebuild the client
    /// set, and atomically swap the state. A single failed client build is
    /// isolated (logged, skipped) and never aborts the refresh (CLAUDE.md §4).
    #[instrument(name = "provider.overlay.refresh", skip_all, err)]
    pub async fn refresh(&self) -> Result<(), ProviderCredentialError> {
        let Some(store) = self.0.store.as_ref() else {
            // Empty overlay: nothing to load, stays platform-only.
            return Ok(());
        };
        // Two independent reads (different tables, no data dependency) — overlap
        // their round-trips rather than serialize them.
        let (creds, defaults) = tokio::try_join!(store.list_all(), store.list_default_models())?;

        let cap = self.0.org_cap;
        let mut state = OverlayState::default();
        let mut dropped_orgs = 0usize;

        // Default models first so a key-less org with a default still appears.
        for (org, model) in defaults {
            if let Some(entry) = entry_for(&mut state, org, cap, &mut dropped_orgs) {
                entry.default_model = Some(model);
            }
        }

        let mut built = 0usize;
        for rec in creds {
            let Some(entry) = entry_for(&mut state, rec.org_id, cap, &mut dropped_orgs) else {
                continue;
            };
            match build_byo_client(rec.provider, &rec.api_key, rec.base_url.as_ref()) {
                Ok(client) => {
                    entry.providers.insert(rec.provider, client);
                    built += 1;
                }
                Err(e) => warn!(
                    patom.org.id = %rec.org_id,
                    patom.provider = rec.provider.as_str(),
                    error = %e,
                    "provider.overlay.client_build_failed",
                ),
            }
        }

        if dropped_orgs > 0 {
            // §5: a silent truncation reads as "covered everything". Surface it.
            warn!(
                patom.provider.overlay.dropped_orgs = dropped_orgs,
                patom.provider.overlay.cap = cap,
                "provider.overlay.org_cap_exceeded",
            );
        }
        tracing::debug!(
            patom.provider.overlay.orgs = state.orgs.len(),
            patom.provider.overlay.clients = built,
            "provider.overlay.refreshed",
        );

        *self.0.state.write().expect("overlay lock poisoned") = state;
        Ok(())
    }
}

/// Borrow (or create) the entry for `org`, honoring the org cap. Returns `None`
/// — and bumps `dropped` — when a *new* org would exceed the cap; orgs already
/// present are always returned so their client set stays complete.
fn entry_for<'a>(
    state: &'a mut OverlayState,
    org: OrgId,
    cap: usize,
    dropped: &mut usize,
) -> Option<&'a mut OrgEntry> {
    if !state.orgs.contains_key(&org) && state.orgs.len() >= cap {
        *dropped += 1;
        return None;
    }
    Some(state.orgs.entry(org).or_default())
}
